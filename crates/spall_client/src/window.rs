//! T19 follow-up (interactive window input): a render window driven by real
//! keyboard/mouse input, connected to a live `sandbox-server` over real QUIC.
//!
//! [`run_interactive_window`] spawns [`crate::net::run_replication_client`] on
//! its own OS thread (an [`crate::interactive::InteractiveSession`] shared
//! between the two: the window writes live [`spall_core::PlayerInput`] into
//! it and reads the predicted player's pose + the replicated terrain back
//! out) and runs the winit event loop on the calling thread, presenting a
//! camera-following view every frame.
//!
//! This is deliberately a small, self-contained debug renderer: solid
//! terrain cells near the player draw as flat-shaded cubes under one
//! directional light — no shadows, no indirect light, no material catalog.
//! Wiring `spall_render`'s real G2 pipeline into a live presented window
//! (rather than its current offscreen-capture use) is out of scope here and
//! left to a follow-up increment.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::Vec3;
use spall_core::{BUTTON_JUMP, GlobalCell};
use spall_physics::CharacterParams;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer,
    materials_from_manifest,
};
use spall_voxel::{Sample, Volume};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};
use yakui_wgpu::{SurfaceInfo as YakuiSurfaceInfo, YakuiWgpu};
use yakui_winit::YakuiWinit;

use crate::ClientError;
use crate::interactive::{InteractiveSession, InteractiveView, LiveInput};
use crate::net::{
    ClientNetConfig, run_replication_client_with_game_content,
    run_replication_client_with_manifest, run_replication_client_with_progression,
};
use crate::predict::CELL_M;
use crate::replica::ReplicaWorld;
use spall_core::MaterialManifest;

/// Debug-view visible radius (metres) around the player's feet. This
/// renderer walks the raw volume every rebuild (no meshing/culling beyond a
/// buried-cell filter), so a wide radius costs real per-rebuild time — but
/// that cost lands on the background `RebuildWorker` thread (see its own
/// doc), not the render/input thread, so a bigger radius only makes terrain
/// pop in a little later after a big camera jump, never stalls a frame.
/// `10.0` (the original value) left almost the whole scene invisible until
/// the player was standing right in front of it — raised after user report.
const VIEW_RADIUS_M: f32 = 48.0;
const VIEW_HEIGHT_UP_M: f32 = 10.0;
const VIEW_HEIGHT_DOWN_M: f32 = 8.0;
/// Rebuild the instanced terrain draw once the player has moved this far
/// (metres) from where it was last built, or the resident terrain changes.
pub const REBUILD_DISTANCE_M: f64 = 1.0;
/// How often, at most, a *stationary* player re-requests a rebuild just to
/// notice a terrain edit landing nearby (a moving player already re-requests
/// every `REBUILD_DISTANCE_M`). This governs background-worker traffic, not
/// frame time — see [`RebuildWorker`] — so it only needs to be "responsive
/// enough for a person to notice", not "cheap".
pub const TERRAIN_RECHECK_INTERVAL: Duration = Duration::from_millis(500);

const MOUSE_SENSITIVITY: f32 = 0.0025;
const MAX_PITCH: f32 = 1.5;

/// Runs an interactively-played `sandbox-client`: connects `net_config` on a
/// background thread and opens a render window on the calling thread. Blocks
/// until the window closes or the network session ends on its own (the
/// server closing, a fatal transport error). `net_config.interactive` is set
/// by this function; any value the caller passed there is overwritten.
pub fn run_interactive_window(net_config: ClientNetConfig) -> Result<(), ClientError> {
    run_interactive_window_with_manifest(net_config, spall_sim::fixtures::stone_manifest())
}

/// Runs the interactive client using the game's material manifest for handshake compatibility.
pub fn run_interactive_window_with_manifest(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
) -> Result<(), ClientError> {
    run_interactive_window_with_game_content(net_config, materials, None)
}

/// Interactive client using material and optional game-asset manifests.
pub fn run_interactive_window_with_game_content(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
) -> Result<(), ClientError> {
    run_interactive_window_with_progression(net_config, materials, asset_manifest_hash, Vec::new())
}

/// Interactive client with optional authenticated progression requests.
pub fn run_interactive_window_with_progression(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
    progression_requests: Vec<spall_protocol::ProgressionRequest>,
) -> Result<(), ClientError> {
    run_interactive_window_with_environment(
        net_config,
        materials,
        asset_manifest_hash,
        progression_requests,
        EnvironmentPreset::Daylight.environment(),
    )
}

/// Interactive client lit by `environment` -- the same lighting model and
/// numbers `spall_render` gives the editor's scene viewport, so a scene reads
/// the same in both. The other `run_interactive_window_*` entry points use
/// [`EnvironmentPreset::Daylight`].
pub fn run_interactive_window_with_environment(
    mut net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
    progression_requests: Vec<spall_protocol::ProgressionRequest>,
    environment: Environment,
) -> Result<(), ClientError> {
    let session = InteractiveSession::new();
    let net_config_streams_residency = net_config.client_residency.is_some();
    net_config.interactive = Some(session.clone());
    // The render table comes from the same manifest the handshake validated,
    // so colour, roughness, metalness and emission match the editor and the
    // capture tools. One extra entry past the manifest ids draws debug
    // overlays.
    let mut render_materials = materials_from_manifest(&materials);
    let debug_material = render_materials.len() as u32;
    render_materials.push(spall_render::Material::new([0.02, 0.9, 0.9], 0.5, 0.0).emissive(2.0));

    // A user event (rather than a plain `()` event loop) so the network
    // thread ending — a failed connect, or the server closing the session —
    // wakes and closes the window right away. Without this the window has no
    // way to learn the session is over: it just keeps presenting an empty
    // scene forever until someone notices and closes it by hand.
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    let net_done = event_loop.create_proxy();

    // Built before the network thread spawns: if this fails there is nothing
    // yet to clean up, whereas failing after would leak a running,
    // never-stopped network thread.
    let mut app = InteractiveApp::new(
        session.clone(),
        environment,
        render_materials,
        debug_material,
        // Only a session that streams bricks can have an absent brick that
        // is genuinely unknown; otherwise absent means empty.
        !net_config_streams_residency,
    )?;

    let net_thread = std::thread::Builder::new()
        .name("spall-client-net".into())
        .spawn(move || {
            let result = if !progression_requests.is_empty() {
                run_replication_client_with_progression(
                    net_config,
                    materials,
                    asset_manifest_hash,
                    progression_requests,
                )
            } else if asset_manifest_hash.is_some() {
                run_replication_client_with_game_content(net_config, materials, asset_manifest_hash)
            } else {
                run_replication_client_with_manifest(net_config, materials)
            };
            let _ = net_done.send_event(());
            result
        })
        .map_err(|e| ClientError::Gpu(format!("spawning network thread: {e}")))?;

    event_loop.set_control_flow(ControlFlow::Poll);
    let run_result = event_loop.run_app(&mut app);

    // The window is gone either way; make sure the network thread notices
    // even if it was the window (not the server) that ended the session.
    session.request_stop();
    let net_result = net_thread
        .join()
        .map_err(|_| ClientError::NetThreadPanicked)?;

    run_result?;
    app.result?;
    net_result.map(|_summary| ()).map_err(ClientError::from)
}

#[derive(Default)]
struct HeldKeys {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    jump: bool,
}

impl HeldKeys {
    /// Local wish direction (see `spall_core::PlayerInput`): `x` strafes
    /// right, `z` drives forward. Diagonal presses are left un-normalized —
    /// `step_character` clamps the resulting world-space wish vector itself.
    fn movement(&self) -> [f32; 3] {
        let mut x = 0.0f32;
        if self.right {
            x += 1.0;
        }
        if self.left {
            x -= 1.0;
        }
        let mut z = 0.0f32;
        if self.forward {
            z += 1.0;
        }
        if self.back {
            z -= 1.0;
        }
        [x, 0.0, z]
    }

    /// Clears the window-side key state and the matching cross-thread input
    /// snapshot as one focus-loss transition.
    fn clear_on_focus_loss(&mut self, input: &LiveInput) {
        *self = Self::default();
        input.clear_held_actions();
    }
}

struct InteractiveApp {
    session: Arc<InteractiveSession>,
    window: Option<Arc<Window>>,
    renderer: Option<WorldRenderer>,
    environment: Environment,
    render_materials: Vec<spall_render::Material>,
    /// Material index of the debug overlay colour (one past the manifest).
    debug_material: u32,
    /// True when the resident terrain instances must be re-uploaded (a
    /// rebuild landed or `F1` toggled visibility).
    terrain_dirty: bool,
    /// The newest sky occupancy the rebuild worker produced, kept so `F5` can
    /// switch visibility-aware skylight off and on without a rebuild.
    last_sky: Option<spall_render::indirect::LightingVolume>,
    /// A fresh `last_sky` (or an `F5` toggle) the renderer has not seen yet.
    sky_dirty: bool,
    /// `F5`: visibility-aware skylight (on) versus the legacy unconditional
    /// hemispheric ambient (off).
    sky_visibility_on: bool,
    /// `F6`: the one-diffuse-bounce term (colour bleed, emissive light).
    bounce_on: bool,
    held: HeldKeys,
    yaw: f32,
    pitch: f32,
    cursor_locked: bool,
    rebuild: RebuildWorker,
    body_worker: BodyWorker,
    /// Centre the *last completed* background rebuild was built around — not
    /// the centre of the most recent request, which may still be in flight.
    last_built_pos: Option<[f64; 3]>,
    last_dispatch_at: Option<Instant>,
    /// One-sample-delayed camera interpolation and correction smoothing.
    /// Keeping the two newest mover publications prevents forward
    /// extrapolation from overshooting the actual stop point.
    camera_follow: CameraFollow,
    hud: Hud,
    /// The last completed background terrain rebuild's instances, kept so
    /// every frame can re-combine them with this frame's *fresh* body
    /// instances (see [`build_body_instances`]) — bodies move continuously
    /// and are cheap to rebuild, so they must not wait on the throttled,
    /// much more expensive terrain rebuild to appear or move.
    last_terrain_instances: Vec<Instance>,
    /// [`BodyWorker`]'s most recently completed result — see that struct's
    /// doc for why this moved off the render thread entirely (first a
    /// blocking lock, then even a `try_lock`-gated build, both measurably
    /// stalled frames under a real ~200-body debris load).
    last_body_draws: Vec<BodyDraw>,
    pose_stats: PoseStats,
    /// `F1`: hide terrain instances entirely, leaving only bodies — useful
    /// for finding debris hidden inside/behind geometry. `F2`: hide body
    /// instances (isolate terrain). Both default on.
    show_terrain: bool,
    show_bodies: bool,
    /// `F3`: draw the player's own predicted capsule bounds (see
    /// [`build_capsule_debug_instances`]) — the closest thing to a collision
    /// outline this renderer's cube-only instancing can produce. Off by
    /// default (adds a small number of bright markers around the player,
    /// which can otherwise obscure the view up close).
    show_capsule: bool,
    result: Result<(), ClientError>,
}

/// Runs [`build_instances`] on a dedicated background thread instead of the
/// window's render/input thread. Every dispatched request gets a freshly
/// walked instance list unconditionally — this no longer consults
/// `ReplicaWorld::terrain_resident_hash()` to decide whether anything
/// actually changed first, because that hash walk costs as much as (or more
/// than) `build_instances` itself (see `perf_probe` below); it's cheaper
/// overall to occasionally rebuild an unchanged scene in the background than
/// to also pay for the "did it change" check.
///
/// This exists because of a measured feedback loop, not just to shave frame
/// time: on a big scene a rebuild can cost 100s of ms (`perf_probe`), and
/// when that ran synchronously in `RedrawRequested`, a slow rebuild delayed
/// the *next* frame, during which the player (still receiving predicted
/// motion from the net thread, which this never blocked) covered more
/// ground before the window got to check again — pushing it straight back
/// past `REBUILD_DISTANCE_M` and into another rebuild. Once a rebuild's cost
/// exceeds roughly `REBUILD_DISTANCE_M` / walking speed, that loop never
/// breaks on its own: FPS collapses further with every step, while the GPU
/// stays idle throughout (this is pure CPU volume-walking, no draw-call
/// cost) and the one thread paying for it doesn't move an aggregate
/// multi-core CPU reading much — which is exactly the "incredible lag, flat
/// CPU/GPU graphs" symptom this was chasing.
///
/// At most one request is in flight at a time (`InteractiveApp` only sends a
/// new one once the last result has been drained); a request superseded by
/// player movement before the worker gets to it is simply answered a little
/// stale; the renderer always has *something* to draw meanwhile, because it
/// keeps presenting the last completed result rather than blocking on a new
/// one.
struct RebuildWorker {
    request_tx: mpsc::Sender<[f64; 3]>,
    result_rx: mpsc::Receiver<RebuildOutcome>,
    in_flight: bool,
}

pub struct RebuildOutcome {
    pub center_m: [f64; 3],
    pub instances: Vec<Instance>,
    /// Sky occupancy built from the same volume snapshot as `instances`, or
    /// `None` when it is identical to the last one sent (nothing to recompute).
    pub sky: Option<spall_render::indirect::LightingVolume>,
    pub sky_stats: crate::sky::SkyOccupancyStats,
    pub elapsed: Duration,
}

/// One rebuild pass: terrain instances plus a sky occupancy grid (only when it
/// actually changed) from `replica`'s current terrain volume around `center_m`.
/// This is exactly what [`RebuildWorker`]'s background thread runs per request
/// — extracted so latency instrumentation can time and drive the real pipeline
/// stage by stage instead of re-implementing it. `sky_anchor` / `last_sky` carry
/// state across calls, same as the worker's loop locals. Returns `None` only
/// when the replica has no terrain yet.
pub fn rebuild_pass(
    replica: &Arc<Mutex<ReplicaWorld>>,
    center_m: [f64; 3],
    sky_anchor: &mut Option<[f64; 3]>,
    last_sky: &mut Option<spall_render::indirect::LightingVolume>,
    absent_is_open: bool,
) -> Option<RebuildOutcome> {
    let volume = replica
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .terrain_volume()
        .cloned()?;
    let start = Instant::now();
    let instances = build_instances(&volume, center_m);
    // Keep the cache where it is until the player has moved far enough to
    // matter, so a stationary or slowly moving player produces an identical
    // grid and no lighting recompute.
    let sky_center = match *sky_anchor {
        Some(anchor) if within_sky_anchor(anchor, center_m) => anchor,
        _ => center_m,
    };
    *sky_anchor = Some(sky_center);
    let (grid, sky_stats) = crate::sky::build_sky_occupancy(&volume, sky_center, absent_is_open);
    let changed = last_sky
        .as_ref()
        .is_none_or(|last| last.origin() != grid.origin() || last.cells() != grid.cells());
    let sky = changed.then(|| {
        *last_sky = Some(grid.clone());
        grid
    });
    Some(RebuildOutcome {
        center_m,
        instances,
        sky,
        sky_stats,
        elapsed: start.elapsed(),
    })
}

impl RebuildWorker {
    /// Spawns the worker thread. It exits on its own once `request_tx`'s
    /// last sender (owned by the `InteractiveApp` this returns into) drops —
    /// no explicit shutdown signal or join needed.
    fn spawn(session: Arc<InteractiveSession>, absent_is_open: bool) -> Result<Self, ClientError> {
        let (request_tx, request_rx) = mpsc::channel::<[f64; 3]>();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("spall-client-rebuild".into())
            .spawn(move || {
                let mut sky_anchor: Option<[f64; 3]> = None;
                let mut last_sky: Option<spall_render::indirect::LightingVolume> = None;
                for center_m in request_rx {
                    let Some(replica) = session.replica.get() else {
                        continue;
                    };
                    let Some(outcome) = rebuild_pass(
                        replica,
                        center_m,
                        &mut sky_anchor,
                        &mut last_sky,
                        absent_is_open,
                    ) else {
                        continue;
                    };
                    if result_tx.send(outcome).is_err() {
                        return; // the window is gone
                    }
                }
            })
            .map_err(|e| ClientError::Gpu(format!("spawning the terrain-rebuild thread: {e}")))?;
        Ok(Self {
            request_tx,
            result_rx,
            in_flight: false,
        })
    }
}

/// Runs [`build_body_instances`] on its own dedicated background thread, at
/// roughly 60 Hz, unconditionally (no request/distance gating — unlike
/// [`RebuildWorker`], bodies need to look freshly-moved every frame, not
/// only after the *player* has moved far). Exists because a first version of
/// body rendering called `build_body_instances` directly in
/// `RedrawRequested`, gated only by a non-blocking `try_lock` on the shared
/// `Mutex<ReplicaWorld>` so it could never *stall waiting for* the lock --
/// but the walk itself (per body: extract its resident bricks, sample every
/// near cell, and for each solid one sample all six neighbours again for the
/// buried-cell check) is real per-frame CPU work, and with a full
/// [`crate::playground`] debris population (~200 bodies) it was measurably
/// too much to redo on the render thread every frame: average frame time
/// climbed past 100 ms while the renderer's own internal timing stayed
/// under 20 ms the whole time -- the cost was real, just outside what that
/// measurement covers. Same fix as terrain's own `RebuildWorker` for the
/// identical shape of problem: move the walk off the thread that has to hit
/// 60 fps, and let the render thread just draw whatever the worker most
/// recently finished.
struct BodyWorker {
    result_rx: mpsc::Receiver<Vec<BodyDraw>>,
}

impl BodyWorker {
    fn spawn(session: Arc<InteractiveSession>) -> Result<Self, ClientError> {
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("spall-client-bodies".into())
            .spawn(move || {
                let mut templates = BodyTemplates::new();
                loop {
                    // Two phases, deliberately: hold the lock only long
                    // enough to read each body's pose (and clone its small
                    // volume *only* when its topology changed since the last
                    // pass — `snapshot_bodies`), then release it *before*
                    // doing any per-cell work (`collect_body_draws`) — see
                    // `snapshot_bodies`'s doc for why the walk itself must
                    // never run while this lock is held.
                    let now = Instant::now();
                    let local_poses = session
                        .local_body_poses
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let views = match session.replica.get() {
                        Some(replica) => {
                            let focus_m = session
                                .view
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .map(|v| v.predicted.position_m);
                            let mut replica = replica.lock().unwrap_or_else(|e| e.into_inner());
                            let render_tick = replica.render_tick(now);
                            snapshot_bodies(
                                &replica,
                                render_tick,
                                focus_m,
                                local_poses.as_ref(),
                                now,
                                &templates,
                            )
                        }
                        None => Vec::new(),
                    };
                    let draws = collect_body_draws(&views, &mut templates);
                    if result_tx.send(draws).is_err() {
                        return; // the window is gone
                    }
                    // Not a hard 60 Hz guarantee (the build above takes real
                    // time too), just a floor so this never busy-spins faster
                    // than the render thread could possibly use it.
                    std::thread::sleep(Duration::from_millis(4));
                }
            })
            .map_err(|e| ClientError::Gpu(format!("spawning the body-instance thread: {e}")))?;
        Ok(Self { result_rx })
    }
}

/// On-screen-debug support (per the ENG-69 lag investigation): there is no
/// text-rendering pipeline in this debug renderer, so "on screen" means the
/// window title bar — plus a mirrored line on stdout so it is visible in
/// whatever terminal launched the client (`cargo xtask play` included).
#[derive(Default)]
struct Hud {
    last_frame_at: Option<Instant>,
    /// Exponential moving average, so a single slow frame doesn't make the
    /// readout unreadable jitter.
    frame_ms_ema: f32,
    frames_since_report: u32,
    last_report_at: Option<Instant>,
    last_rebuild_ms: f32,
    last_rebuild_instances: usize,
    last_sky_stats: crate::sky::SkyOccupancyStats,
    /// Worst single-frame total render time seen since the last report —
    /// see `record_frame`. Reset to `0.0` each time `report` runs.
    max_frame_ms: f32,
    /// Worst single-frame terrain-instance-buffer upload time seen since the
    /// last report (frames without a fresh upload don't count — see
    /// `FrameTiming::buffer_upload_ms`). Reset to `0.0` each time `report`
    /// runs.
    max_buffer_upload_ms: f32,
    max_hud_cpu_ms: f32,
    hud_cpu_sum_ms: f64,
    max_hud_gpu_ms: f32,
    hud_gpu_sum_ms: f64,
    hud_gpu_samples: u32,
    /// `InteractiveView::corrections` as of the last report — a *lifetime*
    /// counter, so the report shows how many are new since then rather than
    /// a running total that only ever grows and stops being useful for
    /// spotting "did one just happen".
    last_corrections_total: u64,
    /// Same idea for `InteractiveView::idle_corrections` — see
    /// `PredictedPlayer::idle_corrections`.
    last_idle_corrections_total: u64,
    /// Same idea for `InteractiveView::unmatched_reconciles` (ENG-69
    /// round 21) — reconciles with no comparison at all, tracked
    /// separately so a large silent resync can never hide behind a
    /// "+0 corrections" line.
    last_unmatched_total: u64,
    /// `InteractiveView::window_stats` as of the last report — same
    /// lifetime-counter-to-delta idea as `last_corrections_total`. ENG-69
    /// round 18: live proof the character-query-window cache is actually
    /// serving sweeps during a hands-on session, not just present in the
    /// code and never exercised.
    last_window_stats: crate::predict::WindowStats,
}

impl Hud {
    const REPORT_INTERVAL: Duration = Duration::from_millis(500);

    /// Call once per `RedrawRequested`, before doing any frame work — updates
    /// the frame-time average and returns `true` on the (throttled) tick
    /// where a report is due.
    fn tick(&mut self, now: Instant) -> bool {
        if let Some(prev) = self.last_frame_at {
            let ms = (now - prev).as_secs_f32() * 1000.0;
            self.frame_ms_ema = if self.frame_ms_ema == 0.0 {
                ms
            } else {
                self.frame_ms_ema * 0.9 + ms * 0.1
            };
        }
        self.last_frame_at = Some(now);
        self.frames_since_report += 1;
        self.last_report_at
            .is_none_or(|t| now - t >= Self::REPORT_INTERVAL)
    }

    fn record_sky(&mut self, stats: crate::sky::SkyOccupancyStats) {
        self.last_sky_stats = stats;
    }

    fn sky_report(&self) -> String {
        let stats = self.last_sky_stats;
        format!(
            "sky occupancy {} resident / {} unknown bricks, {} solid cells",
            stats.resident_bricks, stats.unknown_bricks, stats.solid_cells
        )
    }

    fn record_rebuild(&mut self, elapsed: Duration, instance_count: usize) {
        self.last_rebuild_ms = elapsed.as_secs_f32() * 1000.0;
        self.last_rebuild_instances = instance_count;
    }

    /// Folds one render frame's timing into the worst-case-since-last-report
    /// trackers (see `max_frame_ms`/`max_buffer_upload_ms`) — the averaged
    /// `frame_ms_ema` above hides exactly the short, occasional stall these
    /// exist to surface (ENG-69 round 12: a video review of a hands-on run
    /// showed a "hold, then jump" pattern the average alone didn't explain).
    fn record_frame(&mut self, timing: &FrameTiming) {
        self.max_frame_ms = self.max_frame_ms.max(timing.total_ms);
        self.max_hud_cpu_ms = self.max_hud_cpu_ms.max(timing.hud_cpu_ms);
        self.hud_cpu_sum_ms += f64::from(timing.hud_cpu_ms);
        if let Some(ms) = timing.hud_gpu_ms {
            self.max_hud_gpu_ms = self.max_hud_gpu_ms.max(ms);
            self.hud_gpu_sum_ms += f64::from(ms);
            self.hud_gpu_samples += 1;
        }
        if let Some(ms) = timing.buffer_upload_ms {
            self.max_buffer_upload_ms = self.max_buffer_upload_ms.max(ms);
        }
    }

    /// The due report's text, and resets the report window. `None` fps until
    /// the first `REPORT_INTERVAL` has actually elapsed (avoids a bogus huge
    /// number from a near-zero-duration first window). The correction fields
    /// are `PredictedPlayer`'s own reconciliation-error counters (see
    /// `InteractiveView`) — a felt "snap back" that lines up with these
    /// climbing is a real prediction/authoritative disagreement, not a
    /// rendering artifact. The idle/vertical/horizontal breakdown (ENG-69
    /// round 7) separates a resting-contact disagreement (idle, vertical)
    /// from a collision-sweep one incurred while moving (horizontal) —
    /// `+N corrections (M idle)` with `M` tracking `N` closely, alongside a
    /// vertical max near the overall max, means it reproduces at a dead
    /// stop and is a ground-height/terrain-collider mismatch, not
    /// strafe-into-a-corner sweep divergence.
    #[allow(clippy::too_many_arguments)]
    fn report(
        &mut self,
        now: Instant,
        server_tick: u64,
        corrections_total: u64,
        max_correction_m: f64,
        idle_corrections_total: u64,
        max_idle_correction_m: f64,
        max_vertical_correction_m: f64,
        max_horizontal_correction_m: f64,
        unmatched_total: u64,
        max_unmatched_displacement_m: f64,
        window_stats_total: crate::predict::WindowStats,
    ) -> String {
        let elapsed = self
            .last_report_at
            .map_or(Self::REPORT_INTERVAL, |t| now - t);
        let fps = self.frames_since_report as f32 / elapsed.as_secs_f32();
        let avg_hud_cpu_ms = self.hud_cpu_sum_ms / f64::from(self.frames_since_report.max(1));
        let hud_gpu_report = if self.hud_gpu_samples == 0 {
            "unavailable".to_owned()
        } else {
            format!(
                "{:.3} ms (avg) / {:.3} ms (max)",
                self.hud_gpu_sum_ms / f64::from(self.hud_gpu_samples),
                self.max_hud_gpu_ms
            )
        };
        self.last_report_at = Some(now);
        self.frames_since_report = 0;
        let new_corrections = corrections_total.saturating_sub(self.last_corrections_total);
        self.last_corrections_total = corrections_total;
        let new_idle = idle_corrections_total.saturating_sub(self.last_idle_corrections_total);
        self.last_idle_corrections_total = idle_corrections_total;
        let new_unmatched = unmatched_total.saturating_sub(self.last_unmatched_total);
        self.last_unmatched_total = unmatched_total;
        let max_frame_ms = self.max_frame_ms;
        let max_buffer_upload_ms = self.max_buffer_upload_ms;
        let max_hud_cpu_ms = self.max_hud_cpu_ms;
        self.max_frame_ms = 0.0;
        self.max_buffer_upload_ms = 0.0;
        self.max_hud_cpu_ms = 0.0;
        self.hud_cpu_sum_ms = 0.0;
        self.max_hud_gpu_ms = 0.0;
        self.hud_gpu_sum_ms = 0.0;
        self.hud_gpu_samples = 0;
        // ENG-69 round 18: `window_sweeps` vs `terrain_fallbacks` shows
        // whether the character-query-window cache is actually the thing
        // resolving movement this session, or silently falling back to the
        // whole-resident-set collider the whole time (e.g. because the
        // player has never strayed far enough from the world's streaming
        // edge for a window to build) — either is a real, different fact
        // about *this* session, not visible any other way.
        let new_window_sweeps = window_stats_total
            .window_sweeps
            .saturating_sub(self.last_window_stats.window_sweeps);
        let new_window_rebuilds = window_stats_total
            .window_rebuilds
            .saturating_sub(self.last_window_stats.window_rebuilds);
        let new_terrain_fallbacks = window_stats_total
            .terrain_fallbacks
            .saturating_sub(self.last_window_stats.terrain_fallbacks);
        self.last_window_stats = window_stats_total;
        format!(
            "{fps:.0} fps | frame {:.1} ms (avg) / {max_frame_ms:.1} ms (max) | HUD CPU {avg_hud_cpu_ms:.3} ms (avg) / {max_hud_cpu_ms:.3} ms (max), GPU {hud_gpu_report} | buffer upload {max_buffer_upload_ms:.1} ms (max) | \
             rebuild {:.1} ms ({} instances) | server tick {server_tick} | \
             +{new_corrections} corrections ({new_idle} idle) (lifetime max {max_correction_m:.3} m idle {max_idle_correction_m:.3} m vert {max_vertical_correction_m:.3} m horiz {max_horizontal_correction_m:.3} m) | \
             +{new_unmatched} unmatched (lifetime max displacement {max_unmatched_displacement_m:.3} m) | \
             window cache: {new_window_sweeps} sweeps ({new_window_rebuilds} rebuilt), {new_terrain_fallbacks} terrain fallbacks",
            self.frame_ms_ema, self.last_rebuild_ms, self.last_rebuild_instances,
        )
    }
}

impl InteractiveApp {
    fn new(
        session: Arc<InteractiveSession>,
        environment: Environment,
        render_materials: Vec<spall_render::Material>,
        debug_material: u32,
        absent_is_open: bool,
    ) -> Result<Self, ClientError> {
        let rebuild = RebuildWorker::spawn(session.clone(), absent_is_open)?;
        let body_worker = BodyWorker::spawn(session.clone())?;
        Ok(Self {
            session,
            window: None,
            renderer: None,
            environment,
            render_materials,
            debug_material,
            terrain_dirty: true,
            last_sky: None,
            sky_dirty: false,
            sky_visibility_on: true,
            bounce_on: true,
            held: HeldKeys::default(),
            // Face -Z at spawn, matching `PlayerInput::NEUTRAL`.
            yaw: 0.0,
            pitch: 0.0,
            cursor_locked: false,
            rebuild,
            body_worker,
            last_built_pos: None,
            last_dispatch_at: None,
            camera_follow: CameraFollow::default(),
            hud: Hud::default(),
            last_terrain_instances: Vec::new(),
            last_body_draws: Vec::new(),
            pose_stats: PoseStats::default(),
            show_terrain: true,
            show_bodies: true,
            show_capsule: false,
            result: Ok(()),
        })
    }

    fn view_dir(&self) -> [f32; 3] {
        view_dir_from(self.yaw, self.pitch)
    }

    fn publish_look(&self) {
        self.session.input.set_view_dir(self.view_dir());
    }

    fn publish_movement(&self) {
        self.session.input.set_movement(self.held.movement());
    }

    /// Releases local intent after the operating system moves focus away from
    /// this window. `winit` does not guarantee matching release events for
    /// keys/buttons that were down at focus loss, so carrying `held` across
    /// that boundary could make the server receive a stale walk or jump.
    fn clear_held_actions(&mut self) {
        self.held.clear_on_focus_loss(&self.session.input);
    }

    fn set_cursor_locked(&mut self, locked: bool) {
        let Some(window) = &self.window else { return };
        if locked {
            let grabbed = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            window.set_cursor_visible(!grabbed);
            self.cursor_locked = grabbed;
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.cursor_locked = false;
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: ClientError) {
        self.result = Err(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for InteractiveApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        event_loop.listen_device_events(DeviceEvents::Always);
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Spall sandbox — interactive")
                .with_inner_size(PhysicalSize::new(1280, 720)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => return self.fail(event_loop, ClientError::Gpu(error.to_string())),
        };
        match WorldRenderer::new(window.clone(), self.environment, &self.render_materials) {
            Ok(renderer) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
                self.set_cursor_locked(true);
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        // Forward every native window event to Yakui first. Its return value
        // tells gameplay input whether the UI consumed this event.
        let ui_consumed = self
            .renderer
            .as_mut()
            .is_some_and(|renderer| renderer.handle_window_event(&event));
        match event {
            WindowEvent::CloseRequested => {
                self.session.request_stop();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
            }
            WindowEvent::Focused(false) => {
                self.clear_held_actions();
                self.set_cursor_locked(false);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !self.cursor_locked && !ui_consumed => {
                self.set_cursor_locked(true);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if ui_consumed {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let held = event.state == ElementState::Pressed;
                match code {
                    KeyCode::KeyW => self.held.forward = held,
                    KeyCode::KeyS => self.held.back = held,
                    KeyCode::KeyA => self.held.left = held,
                    KeyCode::KeyD => self.held.right = held,
                    KeyCode::Space => {
                        self.held.jump = held;
                        self.session.input.set_button(BUTTON_JUMP, held);
                    }
                    KeyCode::Escape if held && !event.repeat => {
                        self.set_cursor_locked(false);
                    }
                    KeyCode::F1 if held && !event.repeat => {
                        self.show_terrain = !self.show_terrain;
                        self.terrain_dirty = true;
                        println!(
                            "spall-interactive: terrain instances {}",
                            if self.show_terrain { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    KeyCode::F2 if held && !event.repeat => {
                        self.show_bodies = !self.show_bodies;
                        println!(
                            "spall-interactive: body instances {}",
                            if self.show_bodies { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    KeyCode::F3 if held && !event.repeat => {
                        self.show_capsule = !self.show_capsule;
                        println!(
                            "spall-interactive: capsule debug markers {}",
                            if self.show_capsule { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    KeyCode::F5 if held && !event.repeat => {
                        self.sky_visibility_on = !self.sky_visibility_on;
                        self.sky_dirty = true;
                        println!(
                            "spall-interactive: visibility-aware skylight {}",
                            if self.sky_visibility_on {
                                "ON"
                            } else {
                                "OFF (legacy unconditional ambient)"
                            }
                        );
                        return;
                    }
                    KeyCode::F6 if held && !event.repeat => {
                        self.bounce_on = !self.bounce_on;
                        if let Some(renderer) = &self.renderer {
                            renderer
                                .scene
                                .set_bounce_enabled(&renderer.queue, self.bounce_on);
                        }
                        println!(
                            "spall-interactive: diffuse bounce {}",
                            if self.bounce_on { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    KeyCode::F4 if held && !event.repeat => {
                        if let Some(renderer) = &mut self.renderer {
                            let view = next_debug_view(renderer.debug_view);
                            renderer.debug_view = view;
                            println!("spall-interactive: render view {}", view.stem());
                        }
                        return;
                    }
                    _ => return,
                }
                self.publish_movement();
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let due_for_report = self.hud.tick(now);

                // Drain the background worker's result, if a fresh one has
                // landed since the last frame (never blocks — `try_recv`).
                // Only the newest matters if somehow more than one queued up.
                while let Ok(outcome) = self.rebuild.result_rx.try_recv() {
                    self.hud
                        .record_rebuild(outcome.elapsed, outcome.instances.len());
                    self.last_built_pos = Some(outcome.center_m);
                    self.last_terrain_instances = outcome.instances;
                    self.terrain_dirty = true;
                    if let Some(sky) = outcome.sky {
                        self.last_sky = Some(sky);
                        self.sky_dirty = true;
                    }
                    self.hud.record_sky(outcome.sky_stats);
                    self.rebuild.in_flight = false;
                }

                let view = *self.session.view.lock().unwrap_or_else(|e| e.into_inner());

                if let Some(v) = view {
                    let feet = v.predicted.position_m;
                    let moved_far_enough = self.last_built_pos.is_none_or(|p| {
                        let d = [feet[0] - p[0], feet[1] - p[1], feet[2] - p[2]];
                        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() >= REBUILD_DISTANCE_M
                    });
                    let due_for_recheck = self
                        .last_dispatch_at
                        .is_none_or(|t| now - t >= TERRAIN_RECHECK_INTERVAL);
                    // At most one request in flight — a faster player than the
                    // worker can keep up with just rides on a slightly stale
                    // draw rather than queuing requests it'll never need.
                    if !self.rebuild.in_flight
                        && (moved_far_enough || due_for_recheck)
                        && self.rebuild.request_tx.send(feet).is_ok()
                    {
                        self.rebuild.in_flight = true;
                        self.last_dispatch_at = Some(now);
                    }
                }

                // Bodies rebuild fresh every frame (cheap — never more than
                // each live body's own resident bricks) and get combined
                // with the last completed (throttled, background) terrain
                // rebuild, so the combined buffer is re-uploaded every frame
                // regardless of whether terrain itself changed. Bodies move
                // continuously; waiting on the terrain rebuild cadence to
                // show that would make them look like they teleport between
                // rebuilds instead of falling.
                // Drain the body worker the same way as the terrain one
                // above: never blocks, only the newest result matters.
                while let Ok(draws) = self.body_worker.result_rx.try_recv() {
                    self.last_body_draws = draws;
                }
                let overlay = if self.show_capsule
                    && let Some(v) = view
                {
                    build_capsule_debug_instances(v.predicted.position_m, self.debug_material)
                } else {
                    Vec::new()
                };
                // Terrain lives in a resident GPU buffer and is re-uploaded
                // only when a rebuild landed or its visibility toggled --
                // not every frame. Bodies move continuously, so they are
                // re-posed (into their own reused buffer) every frame.
                let empty = Vec::new();
                let terrain = if !self.terrain_dirty {
                    None
                } else if self.show_terrain {
                    Some(&self.last_terrain_instances)
                } else {
                    Some(&empty)
                };
                let uploading_terrain = terrain.is_some();
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                if self.sky_dirty {
                    let occupancy = self.last_sky.as_ref().filter(|_| self.sky_visibility_on);
                    // Nothing to upload yet (no rebuild has landed) keeps the
                    // dirty flag so the first occupancy is not lost.
                    if occupancy.is_some() || !self.sky_visibility_on {
                        renderer.scene.set_sky_occupancy(
                            &renderer.device,
                            &renderer.queue,
                            occupancy,
                        );
                        self.sky_dirty = false;
                    }
                }
                let outcome = match renderer.begin_frame(terrain.map(Vec::as_slice), &overlay) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
                };
                // `begin_frame` uploaded the terrain before it could fail or
                // skip, so the resident buffer is current either way.
                if uploading_terrain {
                    self.terrain_dirty = false;
                }
                let timing = match outcome {
                    AcquireOutcome::Skipped(timing) => timing,
                    AcquireOutcome::Ready(acquired) => {
                        // Read the freshest pose and compute the camera with
                        // a timestamp taken right now — after the swapchain
                        // acquire's variable-length wait above, not before
                        // it. ENG-69 round 13: `interactive-frames.jsonl`
                        // showed that wait ranging ~1-33ms frame to frame
                        // (see `desired_maximum_frame_latency`'s doc); doing
                        // this before `begin_frame`, as this used to, meant
                        // some frames' camera was measurably staler than
                        // others by the time they were actually drawn —
                        // uneven sideways motion during a strafe, most
                        // visible along a nearby object's silhouette.
                        let fresh_view =
                            *self.session.view.lock().unwrap_or_else(|e| e.into_inner());
                        let look_dir = Vec3::from_array(view_dir_from(self.yaw, self.pitch));
                        let render_now = Instant::now();
                        // Client-authoritative bodies are posed here, at the
                        // exact instant the camera is sampled, rather than by
                        // the background worker at some earlier moment: the
                        // worker's result is reused for several frames, so a
                        // body would hold still and then jump while the
                        // camera glided.
                        let local_poses = self
                            .session
                            .local_body_poses
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let mut body_instances = Vec::new();
                        if self.show_bodies {
                            pose_body_instances(
                                &self.last_body_draws,
                                local_poses.as_ref(),
                                render_now,
                                &mut self.pose_stats,
                                &mut body_instances,
                            );
                        }
                        let cam = fresh_view.map(|v| {
                            (
                                self.camera_follow.eye(v, render_now, local_poses.is_none()),
                                look_dir,
                            )
                        });
                        match renderer.finish_frame(acquired, cam.as_ref(), &body_instances, None) {
                            Ok(timing) => timing,
                            Err(error) => {
                                self.fail(event_loop, error);
                                return;
                            }
                        }
                    }
                };
                self.hud.record_frame(&timing);
                if let Some(frames) = &self.session.frames {
                    frames.record(
                        timing.total_ms,
                        timing.buffer_upload_ms,
                        timing.instance_count,
                        timing.acquire_ms,
                        timing.submit_ms,
                        timing.present_ms,
                    );
                }
                if due_for_report {
                    let server_tick = view.map_or(0, |v| v.server_tick);
                    let corrections = view.map_or(0, |v| v.corrections);
                    let max_correction_m = view.map_or(0.0, |v| v.max_correction_m);
                    let idle_corrections = view.map_or(0, |v| v.idle_corrections);
                    let max_idle_correction_m = view.map_or(0.0, |v| v.max_idle_correction_m);
                    let max_vertical_correction_m =
                        view.map_or(0.0, |v| v.max_vertical_correction_m);
                    let max_horizontal_correction_m =
                        view.map_or(0.0, |v| v.max_horizontal_correction_m);
                    let unmatched_reconciles = view.map_or(0, |v| v.unmatched_reconciles);
                    let max_unmatched_displacement_m =
                        view.map_or(0.0, |v| v.max_unmatched_displacement_m);
                    let window_stats = view.map_or_else(Default::default, |v| v.window_stats);
                    let line = self.hud.report(
                        now,
                        server_tick,
                        corrections,
                        max_correction_m,
                        idle_corrections,
                        max_idle_correction_m,
                        max_vertical_correction_m,
                        max_horizontal_correction_m,
                        unmatched_reconciles,
                        max_unmatched_displacement_m,
                        window_stats,
                    );
                    let line = format!("{line} | {}", self.pose_stats.take_report());
                    let line = match view {
                        Some(v) => format!(
                            "{line} | feet ({:.2}, {:.2}, {:.2}) m",
                            v.predicted.position_m[0],
                            v.predicted.position_m[1],
                            v.predicted.position_m[2]
                        ),
                        None => line,
                    };
                    let line = match &self.renderer {
                        Some(renderer) => format!(
                            "{line} | {} | {}",
                            renderer.scene_report(),
                            self.hud.sky_report()
                        ),
                        None => line,
                    };
                    if let Some(window) = &self.window {
                        window.set_title(&format!("Spall sandbox — interactive | {line}"));
                    }
                    eprintln!("spall-interactive: {line}");
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    /// The network thread's wake-up (see `run_interactive_window`): the
    /// session ended, either the server closing it or a failed connect.
    /// `net_result` (checked after `run_app` returns) carries the actual
    /// error, if any — this just makes sure the window doesn't sit there
    /// showing an empty scene once there is no session left to drive it.
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        event_loop.exit();
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.cursor_locked
        {
            self.yaw += delta.0 as f32 * MOUSE_SENSITIVITY;
            self.pitch =
                (self.pitch - delta.1 as f32 * MOUSE_SENSITIVITY).clamp(-MAX_PITCH, MAX_PITCH);
            self.publish_look();
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

/// How quickly the displayed camera position catches up to the interpolated
/// prediction — see [`CameraFollow::eye`].
/// Short enough to add well under a frame's worth of lag to genuinely
/// continuous movement (WASD keeps moving the target every frame, so
/// smoothing barely touches it), long enough to turn a `PredictedPlayer`
/// reconciliation correction — measured live at a small, consistent ~0.15 m,
/// happening even while standing still (resting-contact micro-jitter
/// between the client's and server's independently-computed physics, not a
/// bug introduced by this window) — into a brief, barely-visible glide
/// instead of a snap.
const CORRECTION_SMOOTHING_TAU_S: f32 = 0.05;

/// Render-follow state for the two independently paced mover/render clocks.
///
/// The old camera extrapolated the newest pose by `velocity * elapsed`. That
/// hid the clocks' beat pattern while movement continued, but necessarily put
/// the camera ahead of the simulation. On the first zero-velocity publication
/// after releasing WASD, its target jumped back to the actual stop position —
/// the small backward jerk observed in UAT. Interpolating from the previous
/// publication to the newest one is one sample later, but never invents travel
/// beyond a position the predictor actually reached.
#[derive(Default)]
struct CameraFollow {
    previous: Option<InteractiveView>,
    current: Option<InteractiveView>,
    displayed: Option<([f64; 3], Instant)>,
}

impl CameraFollow {
    fn target(&mut self, view: InteractiveView, now: Instant) -> [f64; 3] {
        if self
            .current
            .is_none_or(|current| current.published_at != view.published_at)
        {
            self.previous = self.current;
            self.current = Some(view);
        }

        let current = self.current.expect("just installed above");
        let Some(previous) = self.previous else {
            return current.predicted.position_m;
        };
        let sample_span = current
            .published_at
            .saturating_duration_since(previous.published_at)
            .as_secs_f64();
        if sample_span <= f64::EPSILON {
            return current.predicted.position_m;
        }
        let elapsed = now
            .saturating_duration_since(current.published_at)
            .as_secs_f64();
        let t = (elapsed / sample_span).clamp(0.0, 1.0);
        std::array::from_fn(|axis| {
            previous.predicted.position_m[axis]
                + (current.predicted.position_m[axis] - previous.predicted.position_m[axis]) * t
        })
    }

    /// `smooth` applies [`CORRECTION_SMOOTHING_TAU_S`]; it is off for
    /// client-authoritative sessions, which have no corrections to hide and
    /// would otherwise show the camera lagging the bodies it pushes.
    fn eye(&mut self, view: InteractiveView, now: Instant, smooth: bool) -> Vec3 {
        let target = self.target(view, now);
        let feet = match self.displayed {
            Some((prev, prev_at)) if smooth => {
                let dt = now.saturating_duration_since(prev_at).as_secs_f32();
                let factor = 1.0 - (-dt / CORRECTION_SMOOTHING_TAU_S).exp();
                std::array::from_fn(|axis| {
                    prev[axis] + (target[axis] - prev[axis]) * f64::from(factor)
                })
            }
            _ => target,
        };
        self.displayed = Some((feet, now));
        let eye_height_m = f64::from(CharacterParams::DEFAULT.total_height_m()) * 0.9;
        Vec3::new(
            feet[0] as f32,
            (feet[1] + eye_height_m) as f32,
            feet[2] as f32,
        )
    }
}

/// Local wish-independent world-space look direction from yaw/pitch — a free
/// function (not an `InteractiveApp` method) so it can be called from the
/// `RedrawRequested` handler's camera step without borrowing all of `self`
/// while `self.renderer` is already mutably borrowed there — see ENG-69
/// round 13.
fn view_dir_from(yaw: f32, pitch: f32) -> [f32; 3] {
    let (sin_y, cos_y) = yaw.sin_cos();
    let (sin_p, cos_p) = pitch.sin_cos();
    [sin_y * cos_p, sin_p, -cos_y * cos_p]
}

/// Cube instances (one per solid cell within the terrain-mesh cull window) for
/// the terrain around `center_m`. Public alongside `rebuild_pass` for the same
/// reason: instrumentation that needs the real geometry-building pipeline
/// without a background worker.
pub fn build_instances(volume: &Volume, center_m: [f64; 3]) -> Vec<Instance> {
    let cell_m = f64::from(CELL_M);
    let center_cell = GlobalCell::new(
        (center_m[0] / cell_m).floor() as i64,
        (center_m[1] / cell_m).floor() as i64,
        (center_m[2] / cell_m).floor() as i64,
    );
    let horiz = (VIEW_RADIUS_M / CELL_M).ceil() as i64;
    let up = (VIEW_HEIGHT_UP_M / CELL_M).ceil() as i64;
    let down = (VIEW_HEIGHT_DOWN_M / CELL_M).ceil() as i64;

    let mut instances = Vec::new();
    for dz in -horiz..=horiz {
        for dx in -horiz..=horiz {
            for dy in -down..=up {
                let cell =
                    GlobalCell::new(center_cell.x + dx, center_cell.y + dy, center_cell.z + dz);
                let Ok(Sample::Filled(material)) = volume.sample(cell) else {
                    continue;
                };
                if is_buried(volume, cell) {
                    continue;
                }
                instances.push(Instance {
                    offset: [
                        ((cell.x as f64 + 0.5) * cell_m) as f32,
                        ((cell.y as f64 + 0.5) * cell_m) as f32,
                        ((cell.z as f64 + 0.5) * cell_m) as f32,
                    ],
                    material: u32::from(material.0),
                    size: [CELL_M; 3],
                    _pad: 0.0,
                    rotation: IDENTITY_ROTATION,
                });
            }
        }
    }
    instances
}

/// One body as [`BodyWorker`] sees it for a single pass: identity, the
/// topology revision its cached template is keyed on, its volume (cloned only
/// on a template miss), and where to draw it.
struct BodyView {
    entity: u64,
    revision: u64,
    volume: Option<Volume>,
    translation_m: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    rotation: [f32; 4],
    /// Network bodies only: what the renderer needs to re-pose at draw time.
    net: Option<NetPose>,
}

/// A replicated body's motion history plus the render clock and focus it was
/// sampled against, so the render thread can pose it at its own frame time.
#[derive(Clone)]
struct NetPose {
    sampler: crate::replica::BodySampler,
    /// Fractional server tick of the replica's render clock at `sampled_at`.
    tick_at_sample: f64,
    sampled_at: Instant,
    focus_m: Option<[f64; 3]>,
}

impl NetPose {
    fn render_tick(&self, now: Instant) -> f64 {
        self.tick_at_sample
            + now.saturating_duration_since(self.sampled_at).as_secs_f64() * self.sampler.hz()
    }
}

/// Per-body cell instances in body-local space, keyed by topology revision.
/// A body's cells only change when its topology does, so the per-cell volume
/// walk (thousands of `sample` calls with ~200 debris bodies) happens once per
/// topology, not once per frame; each frame only re-poses the cached cells.
type BodyTemplates = std::collections::HashMap<u64, (u64, Arc<Vec<Instance>>)>;

/// A body's cached cells plus the pose the worker last saw for it. The render
/// thread re-poses `template` itself at draw time (see
/// [`pose_body_instances`]); the worker's pose is only the fallback for bodies
/// with no locally simulated pose.
struct BodyDraw {
    entity: u64,
    template: Arc<Vec<Instance>>,
    translation_m: [f64; 3],
    rotation: [f32; 4],
    net: Option<NetPose>,
}

/// Every live body's pose (and, only when its cached template is missing or
/// stale, a clone of its volume), read from the locked replica —
/// deliberately the *only* thing [`BodyWorker`] does while holding the lock,
/// same reason [`RebuildWorker`] clones the terrain volume out instead of
/// walking it locked (see that struct's doc). An earlier version had
/// [`build_body_instances`] itself take `&ReplicaWorld` and do the whole
/// per-cell walk while still holding the lock: moving that walk to its own
/// thread stopped it from stalling the *render* thread, but the walk still
/// held the lock for its entire duration every ~16 ms, which measurably
/// stalled the *network/prediction* thread instead — same underlying mistake
/// (expensive work performed while a shared lock most other threads also
/// need is held), just relocated to a different pair of threads and a
/// different symptom (predicted movement stuttering — "move, pause, move,
/// pause" while holding a direction key — instead of dropped frames).
///
/// `local_poses` (testing-only client authority) is sampled at `now`, so
/// locally simulated bodies interpolate between fixed physics steps.
fn snapshot_bodies(
    replica: &ReplicaWorld,
    render_tick: f64,
    focus_m: Option<[f64; 3]>,
    local_poses: Option<&crate::interactive::LocalBodyPoses>,
    now: Instant,
    templates: &BodyTemplates,
) -> Vec<BodyView> {
    replica
        .body_volumes()
        .filter_map(|(entity, volume_id)| {
            let volume = replica.volume(volume_id)?;
            let raw = entity.get();
            let mut net = None;
            let (translation_m, rotation) =
                match local_poses.and_then(|poses| poses.sample(raw, now)) {
                    Some(pose) => (pose.translation_m, pose.rotation),
                    None => {
                        let sampler = replica.body_sampler(entity)?;
                        let pose = sampler.presented(render_tick, focus_m)?;
                        net = Some(NetPose {
                            sampler,
                            tick_at_sample: render_tick,
                            sampled_at: now,
                            focus_m,
                        });
                        (
                            pose.translation_m,
                            pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]),
                        )
                    }
                };
            let revision = volume.next_revision().get();
            let cached = templates.get(&raw).is_some_and(|(rev, _)| *rev == revision);
            Some(BodyView {
                entity: raw,
                revision,
                volume: (!cached).then(|| volume.clone()),
                translation_m,
                rotation,
                net,
            })
        })
        .collect()
}

/// Body-local cube positions/colours for `volume` (no pose applied).
fn build_body_template(volume: &Volume) -> Vec<Instance> {
    let mut instances = Vec::new();
    let bricks = volume.resident_brick_coords();
    if bricks.is_empty() {
        return instances;
    }
    let (mut min, mut max) = (bricks[0], bricks[0]);
    for b in &bricks {
        min.x = min.x.min(b.x);
        min.y = min.y.min(b.y);
        min.z = min.z.min(b.z);
        max.x = max.x.max(b.x);
        max.y = max.y.max(b.y);
        max.z = max.z.max(b.z);
    }
    let edge = spall_core::BRICK_EDGE as i64;
    let cell_min = GlobalCell::new(min.x * edge, min.y * edge, min.z * edge);
    let cell_max = GlobalCell::new(
        max.x * edge + edge - 1,
        max.y * edge + edge - 1,
        max.z * edge + edge - 1,
    );
    let cell_m = volume.cell_size().metres();
    for gz in cell_min.z..=cell_max.z {
        for gy in cell_min.y..=cell_max.y {
            for gx in cell_min.x..=cell_max.x {
                let cell = GlobalCell::new(gx, gy, gz);
                let Ok(Sample::Filled(material)) = volume.sample(cell) else {
                    continue;
                };
                // No `is_buried` check here (unlike terrain's own
                // `build_instances`): these bodies are at most a few
                // cells per axis, so buried interior cells are rare and
                // small in number, while the check itself costs up to
                // six more `volume.sample` calls per solid cell —
                // proportionally far more expensive here than for
                // terrain's much larger, mostly-interior volumes. A few
                // wasted, fully-occluded cubes are cheaper than the
                // lookups that would have culled them.
                instances.push(Instance {
                    offset: [
                        ((cell.x as f64 + 0.5) * cell_m) as f32,
                        ((cell.y as f64 + 0.5) * cell_m) as f32,
                        ((cell.z as f64 + 0.5) * cell_m) as f32,
                    ],
                    material: u32::from(material.0),
                    size: [cell_m as f32; 3],
                    _pad: 0.0,
                    rotation: IDENTITY_ROTATION,
                });
            }
        }
    }
    instances
}

/// Every detached body's cached cells and last-seen pose, built from
/// [`snapshot_bodies`]'s output entirely after the replica lock has been
/// released (see that function's doc for why this split exists). Nothing in
/// this renderer built body instances before this feature (`build_instances`
/// only ever walks the *terrain* volume) — a hands-on playground session with
/// a live debris spawner found this the hard way: bodies were replicating and
/// reconciling correctly the whole time, just never once drawn.
fn collect_body_draws(views: &[BodyView], templates: &mut BodyTemplates) -> Vec<BodyDraw> {
    let live: std::collections::HashSet<u64> = views.iter().map(|v| v.entity).collect();
    templates.retain(|entity, _| live.contains(entity));

    let mut draws = Vec::with_capacity(views.len());
    for view in views {
        if let Some(volume) = &view.volume {
            templates.insert(
                view.entity,
                (view.revision, Arc::new(build_body_template(volume))),
            );
        }
        let Some((_, template)) = templates.get(&view.entity) else {
            continue;
        };
        draws.push(BodyDraw {
            entity: view.entity,
            template: template.clone(),
            translation_m: view.translation_m,
            rotation: view.rotation,
            net: view.net.clone(),
        });
    }
    draws
}

/// Poses every body's cached cells for drawing at `now`: the locally
/// simulated pose interpolated to `now` when there is one, else the pose the
/// worker last saw. Each cube gets the body's full rotation (its centre is
/// rotated and so is its mesh), not just its centre.
fn pose_body_instances(
    draws: &[BodyDraw],
    local_poses: Option<&crate::interactive::LocalBodyPoses>,
    now: Instant,
    stats: &mut PoseStats,
    out: &mut Vec<Instance>,
) {
    for draw in draws {
        let (translation_m, rotation) = match local_poses.and_then(|p| p.sample(draw.entity, now)) {
            Some(pose) => (pose.translation_m, pose.rotation),
            None => match &draw.net {
                Some(net) => {
                    let tick = net.render_tick(now);
                    match net.sampler.presented(tick, net.focus_m) {
                        Some(pose) => {
                            if net.sampler.is_moving() {
                                let age_ms = net.sampler.latest_age_ticks(tick).unwrap_or(0.0)
                                    / net.sampler.hz()
                                    * 1000.0;
                                stats.record(draw.entity, pose.translation_m, age_ms);
                            }
                            (
                                pose.translation_m,
                                pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]),
                            )
                        }
                        None => (draw.translation_m, draw.rotation),
                    }
                }
                None => (draw.translation_m, draw.rotation),
            },
        };
        let [rx, ry, rz, rw] = rotation;
        let quat = glam::Quat::from_xyzw(rx, ry, rz, rw);
        let translation = Vec3::new(
            translation_m[0] as f32,
            translation_m[1] as f32,
            translation_m[2] as f32,
        );
        out.extend(draw.template.iter().map(|cell| Instance {
            offset: (quat * Vec3::from_array(cell.offset) + translation).to_array(),
            rotation,
            ..*cell
        }));
    }
}

/// Draw-time pose diagnostics for moving network bodies: how old the newest
/// snapshot is at each draw, and how often a moving body is drawn at exactly
/// the pose it had the previous frame (a visible hold).
#[derive(Default)]
struct PoseStats {
    last: std::collections::HashMap<u64, [f64; 3]>,
    draws: u64,
    repeats: u64,
    age_sum_ms: f64,
    age_max_ms: f64,
}

impl PoseStats {
    fn record(&mut self, entity: u64, translation_m: [f64; 3], age_ms: f64) {
        self.draws += 1;
        self.age_sum_ms += age_ms;
        self.age_max_ms = self.age_max_ms.max(age_ms);
        if self.last.insert(entity, translation_m) == Some(translation_m) {
            self.repeats += 1;
        }
    }

    /// Report text; resets the counters (not the per-entity history).
    fn take_report(&mut self) -> String {
        let text = if self.draws == 0 {
            "net bodies: none moving".to_string()
        } else {
            format!(
                "net bodies: {} moving draws, {} repeated poses, snapshot age {:.0} ms (avg) / {:.0} ms (max)",
                self.draws,
                self.repeats,
                self.age_sum_ms / self.draws as f64,
                self.age_max_ms
            )
        };
        self.draws = 0;
        self.repeats = 0;
        self.age_sum_ms = 0.0;
        self.age_max_ms = 0.0;
        text
    }
}

/// One thin axis-aligned cuboid from `a` to `b`. Capsule-box edges are all
/// axis aligned, so this gives the cube-only debug renderer true continuous
/// wire-like strokes without adding a separate line pipeline.
fn debug_line(a: Vec3, b: Vec3, material: u32) -> impl Iterator<Item = Instance> {
    const THICKNESS_M: f32 = 0.0125;
    let delta = (b - a).abs();
    let mut dimensions = [THICKNESS_M; 3];
    let axis = if delta.x >= delta.y && delta.x >= delta.z {
        0
    } else if delta.y >= delta.z {
        1
    } else {
        2
    };
    dimensions[axis] = delta[axis] + THICKNESS_M;
    let midpoint = (a + b) * 0.5;
    std::iter::once(Instance {
        offset: midpoint.to_array(),
        material,
        size: dimensions,
        _pad: 0.0,
        rotation: IDENTITY_ROTATION,
    })
}

/// `F3`: a stand-in for real collision-shape wireframes. This renderer has
/// no line/wireframe pipeline at all (see the module doc: a single unit-cube
/// mesh instanced by position + colour, nothing else) -- building one is a
/// real render-pipeline change, out of scope for a debug toggle. This reuses
/// the existing instancing mechanism instead: very thin cuboids along all
/// twelve edges of the player's actual collision bounds
/// (`CharacterParams::DEFAULT`'s capsule, approximated here as its own
/// bounding box -- `0.6 m x 0.6 m` footprint, `1.8 m` tall -- since the
/// capsule's rounded ends are a much smaller visual difference than "is
/// there an outline here at all"). It is not a raster-line primitive, but the
/// 1.25 cm strokes read as a wireframe instead of voxel-sized bars and show
/// exactly where the client believes its own collision volume is relative
/// to the terrain and debris around it, which is the concrete, useful
/// question "collision outlines" is usually really asking. A first version
/// only drew the four vertical edges (no top/bottom rings connecting them),
/// which read as four floating dashed lines rather than anything box-shaped
/// -- the full 12-edge wireframe below is what actually looks like "a body
/// cube."
fn build_capsule_debug_instances(feet_m: [f64; 3], material: u32) -> Vec<Instance> {
    let params = CharacterParams::DEFAULT;
    let r = f64::from(params.radius_m) as f32;
    let height = f64::from(params.total_height_m()) as f32;
    let feet = Vec3::new(feet_m[0] as f32, feet_m[1] as f32, feet_m[2] as f32);

    // The box's 8 corners: bottom ring (y=0) then top ring (y=height), each
    // in (+x+z, +x-z, -x-z, -x+z) order.
    let corner = |dx: f32, dz: f32, dy: f32| feet + Vec3::new(dx * r, dy * height, dz * r);
    let bottom = [
        corner(1.0, 1.0, 0.0),
        corner(1.0, -1.0, 0.0),
        corner(-1.0, -1.0, 0.0),
        corner(-1.0, 1.0, 0.0),
    ];
    let top = [
        corner(1.0, 1.0, 1.0),
        corner(1.0, -1.0, 1.0),
        corner(-1.0, -1.0, 1.0),
        corner(-1.0, 1.0, 1.0),
    ];

    let mut instances = Vec::new();
    for i in 0..4 {
        let j = (i + 1) % 4;
        instances.extend(debug_line(bottom[i], bottom[j], material)); // bottom ring
        instances.extend(debug_line(top[i], top[j], material)); // top ring
        instances.extend(debug_line(bottom[i], top[i], material)); // vertical edge
    }
    instances
}

/// A cell whose six face neighbours are all solid contributes no visible
/// surface; skipping it keeps the instance count near the visible shell
/// instead of the whole solid volume.
fn is_buried(volume: &Volume, cell: GlobalCell) -> bool {
    const NEIGHBORS: [[i64; 3]; 6] = [
        [1, 0, 0],
        [-1, 0, 0],
        [0, 1, 0],
        [0, -1, 0],
        [0, 0, 1],
        [0, 0, -1],
    ];
    NEIGHBORS.iter().all(|[dx, dy, dz]| {
        matches!(
            volume.sample(GlobalCell::new(cell.x + dx, cell.y + dy, cell.z + dz)),
            Ok(Sample::Filled(_))
        )
    })
}

/// The next debug view in the `F4` cycle, wrapping back to shaded.
fn next_debug_view(view: DebugView) -> DebugView {
    match view {
        DebugView::Shaded => DebugView::Albedo,
        DebugView::Albedo => DebugView::Normals,
        DebugView::Normals => DebugView::Depth,
        DebugView::Depth => DebugView::ShadowCascades,
        DebugView::ShadowCascades => DebugView::ShadowVisibility,
        DebugView::ShadowVisibility => DebugView::SkyVisibility,
        DebugView::SkyVisibility => DebugView::Roughness,
        DebugView::Roughness | DebugView::IndirectOnly => DebugView::Shaded,
    }
}

/// How far (metres) the player may move from where the lighting cache was
/// centred before it is re-centred. The cache reaches 32 m each way and rays 24
/// m, so 8 m of drift keeps every nearby surface well inside it.
const SKY_ANCHOR_DRIFT_M: f64 = 8.0;

fn within_sky_anchor(anchor: [f64; 3], center: [f64; 3]) -> bool {
    let horizontal = (anchor[0] - center[0]).hypot(anchor[2] - center[2]);
    horizontal <= SKY_ANCHOR_DRIFT_M && (anchor[1] - center[1]).abs() <= SKY_ANCHOR_DRIFT_M
}

/// One cube drawn by the shared `spall_render` instanced-cube path. The
/// material is the cell's `MaterialId`, looked up in the manifest-derived
/// table, so colour, roughness, metalness and emission all come from the same
/// definitions the editor and capture tools use.
pub(super) type Instance = CubeInstance;

const IDENTITY_ROTATION: [f32; 4] = CubeInstance::IDENTITY_ROTATION;

/// Vertical field of view of the game camera.
const CAMERA_FOV_Y_DEG: f32 = 75.0;
const CAMERA_Z_NEAR_M: f32 = 0.05;
const CAMERA_Z_FAR_M: f32 = 300.0;

pub(super) struct WorldRenderer {
    environment: Environment,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    /// The shared `spall_render` frame renderer (shadows, HDR, tone map).
    /// Terrain, bodies and debug overlays live in its resident instance
    /// buffers.
    scene: GameRenderer,
    /// Reserved material and instances used only by the ENG-103 inspection
    /// viewer; the normal game material table remains opaque.
    debug_water_material: u32,
    debug_water_instances: Vec<Instance>,
    /// `F4` cycles this through the renderer's debug views.
    debug_view: DebugView,
    /// The last camera basis the window built (`InteractiveApp` computes eye
    /// position / look direction; this struct only knows the surface aspect
    /// ratio needed to finish the projection).
    aspect: f32,
    yakui: yakui::Yakui,
    yakui_winit: YakuiWinit,
    yakui_wgpu: YakuiWgpu,
    yakui_buffers: yakui_wgpu::Buffers,
    yakui_clicks: u64,
    hud_gpu_timer: Option<HudGpuTimer>,
}

struct HudGpuReadback {
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    receiver: Option<mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>>,
}

/// Optional GPU timestamp around the Yakui pass. Readbacks are asynchronous
/// and rotate through three buffers so measurement never stalls presentation.
struct HudGpuTimer {
    queries: wgpu::QuerySet,
    slots: [HudGpuReadback; 3],
    period_ns: f32,
    latest_ms: Option<f32>,
}

impl HudGpuTimer {
    fn new(device: &wgpu::Device, period_ns: f32) -> Self {
        let slots = std::array::from_fn(|_| HudGpuReadback {
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-hud-query-resolve"),
                size: 16,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-hud-query-readback"),
                size: 16,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            receiver: None,
        });
        Self {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("spall-hud-timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            slots,
            period_ns,
            latest_ms: None,
        }
    }

    fn acquire(&mut self) -> (Option<usize>, Option<f32>) {
        let mut completed_ms = None;
        for slot in &mut self.slots {
            let Some(receiver) = slot.receiver.take() else {
                continue;
            };
            match receiver.try_recv() {
                Ok(Ok(())) => {
                    if let Ok(mapped) = slot.readback.slice(..).get_mapped_range() {
                        let ticks = bytemuck::cast_slice::<u8, u64>(&mapped);
                        if ticks.len() >= 2 {
                            let measured = (ticks[1].saturating_sub(ticks[0]) as f32
                                * self.period_ns
                                / 1_000_000.0,);
                            self.latest_ms = Some(measured.0);
                            completed_ms = Some(measured.0);
                        }
                    }
                    slot.readback.unmap();
                }
                Ok(Err(_)) => {}
                Err(mpsc::TryRecvError::Empty) => {
                    slot.receiver = Some(receiver);
                }
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        let index = self.slots.iter().position(|slot| slot.receiver.is_none());
        (index, completed_ms)
    }

    fn write_start(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.write_timestamp(&self.queries, 0);
    }

    fn resolve(&self, encoder: &mut wgpu::CommandEncoder, slot: usize) {
        encoder.write_timestamp(&self.queries, 1);
        encoder.resolve_query_set(&self.queries, 0..2, &self.slots[slot].resolve, 0);
        encoder.copy_buffer_to_buffer(
            &self.slots[slot].resolve,
            0,
            &self.slots[slot].readback,
            0,
            16,
        );
    }

    fn map_after_submit(&mut self, slot: usize) {
        let (sender, receiver) = mpsc::channel();
        self.slots[slot]
            .readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        self.slots[slot].receiver = Some(receiver);
    }
}

/// One render frame's timing breakdown — see [`WorldRenderer::begin_frame`]/
/// [`WorldRenderer::finish_frame`] and `Hud::record_frame`. Added ENG-69
/// round 12: the averaged `frame_ms_ema`
/// the HUD already tracked hides short, occasional stalls; a video review of
/// an actual hands-on run showed a "hold, then jump" pattern (~60% of
/// consecutive frames nearly identical, interspersed with larger jumps) that
/// an average can't diagnose. This breaks a frame into the stages that can
/// plausibly eat a vsync interval's worth of time on their own.
pub(super) struct FrameTiming {
    /// Wall time for the whole `render` call.
    total_ms: f32,
    /// `Some` only on the (occasional) frame a background `RebuildWorker`
    /// result landed and `begin_frame` uploaded a fresh instance buffer —
    /// see `create_buffer_init` there. This reallocates and copies the
    /// *entire* buffer every time rather than reusing one; round 12
    /// suspected this (a bigger buffer since `VIEW_RADIUS_M` went 10m ->
    /// 48m) as the stall's cause, but round 13's data disproved it — this
    /// stayed under 0.2ms on every measured frame, stalled or not. Kept
    /// for visibility, not because it's still a live suspect.
    buffer_upload_ms: Option<f32>,
    /// Instance count uploaded, when `buffer_upload_ms` is `Some`.
    instance_count: Option<u32>,
    /// Time blocked in `surface.get_current_texture()` — round 13 found
    /// this is where virtually all frame-to-frame timing variance actually
    /// lives (a regular burst pattern from `desired_maximum_frame_latency`
    /// letting the CPU queue ahead of the display; see that field's doc).
    acquire_ms: f32,
    /// Time in `queue.submit()` (usually just enqueues; doesn't normally
    /// wait on the GPU).
    submit_ms: f32,
    /// Time in `surface_texture.present()` — on some backends this, not
    /// `acquire`, is where `PresentMode::Fifo` actually blocks for vsync.
    present_ms: f32,
    hud_cpu_ms: f32,
    hud_gpu_ms: Option<f32>,
}

impl FrameTiming {
    /// A frame that bailed out of `render` early (an `Outdated`/`Lost`/
    /// `Timeout` swapchain acquire) — whatever ran before the early return
    /// still counts, the rest is `0.0`.
    fn early_return(
        total_ms: f32,
        buffer_upload_ms: Option<f32>,
        instance_count: Option<u32>,
        acquire_ms: f32,
    ) -> Self {
        Self {
            total_ms,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
            submit_ms: 0.0,
            present_ms: 0.0,
            hud_cpu_ms: 0.0,
            hud_gpu_ms: None,
        }
    }
}

/// A frame that has acquired its swapchain image, awaiting only the
/// camera-dependent draw ([`WorldRenderer::finish_frame`]) — see
/// [`WorldRenderer::begin_frame`].
pub(super) struct AcquiredFrame {
    frame_start: Instant,
    surface_texture: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    buffer_upload_ms: Option<f32>,
    instance_count: Option<u32>,
    acquire_ms: f32,
}

/// [`WorldRenderer::begin_frame`]'s result: either an acquired frame ready
/// for [`WorldRenderer::finish_frame`], or a frame that bailed out early
/// (an `Outdated`/`Lost`/`Timeout` swapchain acquire) and already has its
/// complete (if mostly-zero) timing.
pub(super) enum AcquireOutcome {
    Ready(AcquiredFrame),
    Skipped(FrameTiming),
}

impl WorldRenderer {
    /// Queue translucent cells for the local fluid inspection view.
    pub(super) fn set_debug_water(&mut self, instances: &[Instance]) {
        self.debug_water_instances.clear();
        self.debug_water_instances
            .extend(instances.iter().copied().map(|mut instance| {
                instance.material = self.debug_water_material;
                instance
            }));
    }

    pub(super) fn new(
        window: Arc<Window>,
        environment: Environment,
        materials: &[spall_render::Material],
    ) -> Result<Self, ClientError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(window.clone()),
        ));
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let timestamp_features =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        let required_features = if adapter.features().contains(timestamp_features) {
            timestamp_features
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("spall-interactive-device"),
            required_features,
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            ..Default::default()
        }))
        .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| ClientError::Gpu("surface exposes no texture format".into()))?;
        let size = window.inner_size();
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities.alpha_modes[0],
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            // ENG-69 round 12: `.local/runs/interactive-frames.jsonl` from a
            // live hands-on run showed a perfectly regular 4-frame cycle —
            // two near-free frames, one ~16ms frame, one ~50ms frame,
            // repeating — with every millisecond of it inside `acquire_ms`
            // (the wait in `get_current_texture`), never in `buffer_upload`/
            // `submit`/`present`. With `ControlFlow::Poll` never yielding
            // between iterations, a latency of `2` let the render loop burst
            // through two queued-ahead frames almost instantly and then stall
            // to let the display drain the backlog, instead of pacing evenly
            // at one vsync interval per frame — the felt "corners jitter"
            // when strafing past an object. `1` forces the CPU to wait for
            // the previous frame to actually present before acquiring the
            // next one, trading a little frame-queuing slack for even
            // pacing.
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &surface_config);
        let aspect = surface_config.width as f32 / surface_config.height.max(1) as f32;
        let yakui = yakui::Yakui::new();
        let yakui_winit = YakuiWinit::new(&window);
        let yakui_wgpu = YakuiWgpu::new(device.clone(), queue.clone());
        let yakui_buffers = yakui_wgpu.buffers();
        let hud_gpu_timer = required_features
            .contains(timestamp_features)
            .then(|| HudGpuTimer::new(&device, queue.get_timestamp_period()));

        let mut render_materials = materials.to_vec();
        let debug_water_material = render_materials.len() as u32;
        let water_color = materials.get(2).copied().unwrap_or_default();
        render_materials.push(water_color.opacity(0.24));
        let scene = GameRenderer::new(
            &device,
            &queue,
            surface_config.format,
            &render_materials,
            (surface_config.width, surface_config.height),
            required_features
                .contains(timestamp_features)
                .then(|| queue.get_timestamp_period()),
        );

        Ok(Self {
            environment,
            device,
            queue,
            surface,
            surface_config,
            scene,
            debug_water_material,
            debug_water_instances: Vec::new(),
            debug_view: DebugView::Shaded,
            aspect,
            yakui,
            yakui_winit,
            yakui_wgpu,
            yakui_buffers,
            yakui_clicks: 0,
            hud_gpu_timer,
        })
    }

    /// One report-line fragment: the shared renderer's per-pass GPU time
    /// (previous completed frames) and resident instance memory.
    fn scene_report(&self) -> String {
        let (terrain, bodies, overlay) = self.scene.instance_counts();
        let memory = self.scene.instance_bytes() as f64 / (1024.0 * 1024.0);
        let sky_memory = self.scene.sky_bytes() as f64 / (1024.0 * 1024.0);
        let sweep = self.scene.lighting_sweep_frames().unwrap_or(0);
        let lit_after = self
            .scene
            .lighting_latency_ms()
            .map_or("n/a".to_owned(), |ms| format!("{ms:.0} ms"));
        match self.scene.pass_timings() {
            Some(t) => format!(
                "scene GPU {:.2} ms (shadow {:.2} / opaque {:.2} / tone {:.2}; sky visibility {} / bounce {} last recompute, {sky_memory:.0} MiB) | lighting sweep {sweep} frames (cache update to last slice recorded {lit_after}, not presented) | cubes {terrain} terrain + {bodies} body + {overlay} overlay ({memory:.1} MiB)",
                t.total_ms(),
                t.shadow_ms,
                t.opaque_ms,
                t.tone_map_ms,
                t.sky_visibility_ms
                    .map_or("n/a".to_owned(), |ms| format!("{ms:.2} ms")),
                t.bounce_ms
                    .map_or("n/a".to_owned(), |ms| format!("{ms:.2} ms"))
            ),
            None => format!(
                "scene GPU timing unavailable | cubes {terrain} terrain + {bodies} body + {overlay} overlay ({memory:.1} MiB)"
            ),
        }
    }

    fn handle_window_event(&mut self, event: &WindowEvent) -> bool {
        self.yakui_winit.handle_window_event(&mut self.yakui, event)
    }

    pub(super) fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
        self.scene.resize(&self.device, size.width, size.height);
        self.aspect = size.width as f32 / size.height as f32;
    }

    /// `eye`/`look_dir` come from the window's own camera state; `scene` is
    /// `None` before the local player has an authoritative pose yet (still
    /// connecting), in which case this just clears the screen.
    /// Uploads any fresh terrain instances and acquires the next swapchain
    /// image — everything in a render frame whose cost doesn't depend on the
    /// camera. Split out from the old single `render` method (ENG-69 round
    /// 13): the caller now computes the camera *after* this returns, with a
    /// fresh timestamp, instead of before — see [`finish_frame`] and
    /// `InteractiveApp`'s `RedrawRequested` handler for why.
    pub(super) fn begin_frame(
        &mut self,
        terrain: Option<&[Instance]>,
        overlay: &[Instance],
    ) -> Result<AcquireOutcome, ClientError> {
        let frame_start = Instant::now();
        let mut buffer_upload_ms = None;
        let mut instance_count = None;
        if let Some(instances) = terrain {
            let upload_start = Instant::now();
            self.scene.set_terrain(&self.device, &self.queue, instances);
            buffer_upload_ms = Some(upload_start.elapsed().as_secs_f32() * 1000.0);
            instance_count = Some(instances.len() as u32);
        }
        self.scene.set_overlay(&self.device, &self.queue, overlay);

        let acquire_start = Instant::now();
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.surface_config);
                drop(t);
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(ClientError::Render(
                    "surface acquisition validation error".into(),
                ));
            }
        };
        let acquire_ms = acquire_start.elapsed().as_secs_f32() * 1000.0;
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        Ok(AcquireOutcome::Ready(AcquiredFrame {
            frame_start,
            surface_texture,
            view,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
        }))
    }

    /// Draws and presents an already-acquired frame. `cam` is `None` before
    /// the local player has an authoritative pose yet (still connecting), in
    /// which case this just clears the screen.
    pub(super) fn finish_frame(
        &mut self,
        acquired: AcquiredFrame,
        cam: Option<&(Vec3, Vec3)>,
        bodies: &[Instance],
        demo_hud: Option<(&str, &str, &str)>,
    ) -> Result<FrameTiming, ClientError> {
        let AcquiredFrame {
            frame_start,
            surface_texture,
            view,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
        } = acquired;

        self.scene.set_bodies(&self.device, &self.queue, bodies);
        if let Some((eye, _)) = cam {
            self.debug_water_instances.sort_by(|a, b| {
                let distance2 = |cube: &Instance| {
                    let p = Vec3::from_array(cube.offset);
                    (p - *eye).length_squared()
                };
                distance2(b).total_cmp(&distance2(a))
            });
        }
        self.scene
            .set_transparent_cubes(&self.device, &self.queue, &self.debug_water_instances);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-interactive-encoder"),
            });
        if let Some((eye, look_dir)) = cam {
            let mut camera = Camera::looking_along(
                *eye,
                *look_dir,
                CAMERA_FOV_Y_DEG.to_radians(),
                self.aspect.max(1e-4),
            );
            camera.z_near = CAMERA_Z_NEAR_M;
            camera.z_far = CAMERA_Z_FAR_M;
            self.scene.render(
                &self.device,
                &self.queue,
                &mut encoder,
                &view,
                &camera,
                &self.environment,
                self.debug_view,
            );
        } else {
            // No pose yet (still connecting): show the environment's
            // background. Written straight to the sRGB surface, so the
            // displayed colour is the environment's exactly.
            let [r, g, b, a] = self.environment.background_linear();
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-interactive-connecting-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
        }
        let hud_start = Instant::now();
        let (hud_gpu_slot, hud_gpu_ms) = self
            .hud_gpu_timer
            .as_mut()
            .map(HudGpuTimer::acquire)
            .unwrap_or((None, None));
        if hud_gpu_slot.is_some()
            && let Some(timer) = self.hud_gpu_timer.as_ref()
        {
            timer.write_start(&mut encoder);
        }
        self.yakui.start();
        let mut clicked = false;
        {
            let clicks = self.yakui_clicks;
            yakui::align(yakui::Alignment::TOP_LEFT, || {
                yakui::column(|| {
                    if let Some((heading, controls, status)) = demo_hud {
                        yakui::text(20.0, heading.to_owned());
                        yakui::text(14.0, controls.to_owned());
                        yakui::text(12.0, status.to_owned());
                    } else {
                        yakui::text(20.0, "SPALL");
                        yakui::text(14.0, "WASD move  |  Space jump  |  Esc release cursor");
                        yakui::text(12.0, "Authoritative multiplayer voxel sandbox");
                        yakui::text(12.0, format!("HUD input test clicks: {clicks}"));
                        clicked = yakui::button("Click to verify UI input").clicked;
                    }
                });
            });
        }
        self.yakui.finish();
        if clicked {
            self.yakui_clicks = self.yakui_clicks.saturating_add(1);
        }
        self.yakui_wgpu.paint_with_encoder(
            &mut self.yakui,
            &mut self.yakui_buffers,
            &mut encoder,
            YakuiSurfaceInfo {
                format: self.surface_config.format,
                sample_count: 1,
                color_attachment: &view,
                resolve_target: None,
            },
        );
        if let Some(slot) = hud_gpu_slot
            && let Some(timer) = self.hud_gpu_timer.as_ref()
        {
            timer.resolve(&mut encoder, slot);
        }
        let hud_cpu_ms = hud_start.elapsed().as_secs_f32() * 1000.0;
        let submit_start = Instant::now();
        self.queue.submit([encoder.finish()]);
        self.scene.finish_timing();
        if let Some(slot) = hud_gpu_slot
            && let Some(timer) = self.hud_gpu_timer.as_mut()
        {
            timer.map_after_submit(slot);
        }
        // A minimal isolated reproduction (`examples/poc_local`, ENG-69)
        // found that on this app's Vulkan backend, `desired_maximum_frame_latency: 1`
        // above did not actually stop the CPU from racing ~2 frames ahead of
        // the display — a measured, sustained 0.2ms/16ms/33ms three-frame
        // burst cycle even at complete idle, which reads as edge
        // ghosting/jitter under any camera motion regardless of what drives
        // it (confirmed independent of physics/input/networking). Blocking
        // here until the GPU has actually finished this frame's work caps
        // one submission in flight at a time and restored a rock-steady
        // ~16.6ms cadence in that reproduction.
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| ClientError::Render(error.to_string()))?;
        let submit_ms = submit_start.elapsed().as_secs_f32() * 1000.0;
        let present_start = Instant::now();
        self.queue.present(surface_texture);
        let present_ms = present_start.elapsed().as_secs_f32() * 1000.0;
        Ok(FrameTiming {
            total_ms: frame_start.elapsed().as_secs_f32() * 1000.0,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
            submit_ms,
            present_ms,
            hud_cpu_ms,
            hud_gpu_ms,
        })
    }
}

#[cfg(test)]
mod input_tests {
    use spall_core::BUTTON_JUMP;

    use super::*;

    fn view(
        position_m: [f64; 3],
        velocity_m_s: [f32; 3],
        published_at: Instant,
    ) -> InteractiveView {
        InteractiveView {
            predicted: spall_physics::CharacterState {
                position_m,
                velocity_m_s,
                grounded: true,
                jump_held_last: false,
            },
            server_tick: 0,
            published_at,
            corrections: 0,
            max_correction_m: 0.0,
            idle_corrections: 0,
            max_idle_correction_m: 0.0,
            max_vertical_correction_m: 0.0,
            max_horizontal_correction_m: 0.0,
            unmatched_reconciles: 0,
            max_unmatched_displacement_m: 0.0,
            window_stats: crate::predict::WindowStats::default(),
        }
    }

    #[test]
    fn focus_loss_clears_window_keys_and_shared_actions_but_keeps_look() {
        let mut held = HeldKeys {
            forward: true,
            left: true,
            jump: true,
            ..HeldKeys::default()
        };
        let input = LiveInput::new();
        input.set_movement(held.movement());
        input.set_view_dir([0.5, 0.25, -0.75]);
        input.set_button(BUTTON_JUMP, true);

        held.clear_on_focus_loss(&input);

        assert_eq!(held.movement(), [0.0; 3]);
        let snapshot = input.snapshot();
        assert_eq!(snapshot.movement, [0.0; 3]);
        assert_eq!(snapshot.buttons, 0);
        assert_eq!(snapshot.view_dir, [0.5, 0.25, -0.75]);
    }

    #[test]
    fn camera_interpolation_does_not_overshoot_and_jerk_back_at_a_stop() {
        let start = Instant::now();
        let tick = Duration::from_millis(16);
        let mut follow = CameraFollow::default();
        let samples = [
            view([0.0, 0.0, 0.0], [4.5, 0.0, 0.0], start),
            view([0.072, 0.0, 0.0], [4.5, 0.0, 0.0], start + tick),
            view([0.144, 0.0, 0.0], [0.0; 3], start + tick * 2),
        ];
        let mut last = f64::NEG_INFINITY;
        for (index, sample) in samples.into_iter().enumerate() {
            for frame in 0..2 {
                let now = start + tick * index as u32 + Duration::from_millis(frame * 8);
                let x = follow.target(sample, now)[0];
                assert!(x >= last, "camera target moved backward: {last} -> {x}");
                assert!(x <= sample.predicted.position_m[0] + f64::EPSILON);
                last = x;
            }
        }
    }

    #[test]
    fn the_lighting_cache_only_recentres_after_real_movement() {
        let anchor = [10.0, 1.0, 10.0];
        assert!(within_sky_anchor(anchor, [10.5, 1.0, 12.0]));
        assert!(
            within_sky_anchor(anchor, [15.0, 1.0, 16.0]),
            "6.4 m of drift is fine"
        );
        assert!(!within_sky_anchor(anchor, [10.0, 1.0, 19.0]));
        assert!(!within_sky_anchor(anchor, [10.0, 10.0, 10.0]));
    }

    #[test]
    fn the_debug_view_cycle_visits_every_window_view_and_wraps_to_shaded() {
        let mut view = DebugView::Shaded;
        let mut seen = vec![view];
        for _ in 0..8 {
            view = next_debug_view(view);
            seen.push(view);
        }
        assert_eq!(seen.first(), seen.last(), "wraps after eight presses");
        seen.pop();
        seen.sort_by_key(|v| v.stem());
        seen.dedup();
        assert_eq!(seen.len(), 8, "no view repeats within a cycle");
    }

    #[test]
    fn capsule_debug_edges_are_thin_continuous_wire_strokes() {
        let lines = build_capsule_debug_instances([0.0; 3], 99);
        assert_eq!(lines.len(), 12, "one cuboid per bounding-box edge");
        for line in lines {
            let dimensions = line.size;
            let thin_axes = dimensions
                .into_iter()
                .filter(|dimension| *dimension <= 0.013)
                .count();
            assert_eq!(thin_axes, 2, "line dimensions were {dimensions:?}");
        }
    }

    #[test]
    fn locally_simulated_pose_reaches_body_render_instances() {
        let now = Instant::now();
        let draws = [BodyDraw {
            entity: 7,
            template: Arc::new(vec![Instance::new(
                [0.0, 0.0, 0.0],
                1,
                [CELL_M; 3],
                [0.0, 0.0, 0.0, 1.0],
            )]),
            translation_m: [2.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            net: None,
        }];
        let local_pose = crate::interactive::LocalPose {
            translation_m: [8.0, 1.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
        };
        let local = crate::interactive::LocalBodyPoses {
            prev: Default::default(),
            curr: [(7, local_pose)].into_iter().collect(),
            curr_at: now,
            step: Duration::from_millis(16),
        };
        let mut instances = Vec::new();
        pose_body_instances(
            &draws,
            Some(&local),
            now,
            &mut PoseStats::default(),
            &mut instances,
        );

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].offset, [8.0, 1.0, 3.0]);
    }
}

#[cfg(test)]
mod perf_probe {
    //! Informational only (no assertions — debug-vs-release and per-machine
    //! variance make a hard threshold meaningless here): times the one thing
    //! `RedrawRequested` does synchronously on the window's own thread that
    //! scales with view radius. Run with `cargo test -p spall_client
    //! build_instances_timing -- --nocapture --test-threads=1` to see it.
    use super::*;
    use crate::replica::{ReplicaConfig, ReplicaWorld};
    use spall_core::VolumeId;

    #[test]
    fn build_instances_timing_walk_arena() {
        let volume = spall_voxel::fixtures::walk_arena(VolumeId::new(1).unwrap());
        // On the floor (top surface y = 1.0 m) near the spawn end.
        let start = std::time::Instant::now();
        let instances = build_instances(&volume, [0.5, 1.0, 2.0]);
        eprintln!(
            "build_instances(walk_arena): {:?}, {} instances",
            start.elapsed(),
            instances.len()
        );
    }

    #[test]
    fn build_instances_timing_g1_full_envelope() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        // A real G1_WORKLOAD_SPAWNS point (spall_sim::fixtures), on the
        // ground rather than in mid-air.
        let start = std::time::Instant::now();
        let instances = build_instances(&volume, [2.0, 13.5, 2.0]);
        eprintln!(
            "build_instances(g1_full_envelope): {:?}, {} instances",
            start.elapsed(),
            instances.len()
        );
    }

    /// `build_scene` calls this *every `RedrawRequested`*, not just on a
    /// rebuild — unlike `build_instances` (throttled to once per metre moved
    /// / terrain change), this is the per-frame floor cost of the debug
    /// window regardless of camera movement.
    #[test]
    fn terrain_resident_hash_timing_g1_full_envelope() {
        let replica = ReplicaWorld::from_baseline(
            spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap()),
            ReplicaConfig::default(),
        );
        let start = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(replica.terrain_resident_hash());
        }
        eprintln!(
            "terrain_resident_hash(g1_full_envelope): {:?}/call over 10 calls",
            start.elapsed() / 10
        );
    }

    #[test]
    fn terrain_resident_hash_timing_walk_arena() {
        let replica = ReplicaWorld::from_baseline(
            spall_voxel::fixtures::walk_arena(VolumeId::new(1).unwrap()),
            ReplicaConfig::default(),
        );
        let start = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(replica.terrain_resident_hash());
        }
        eprintln!(
            "terrain_resident_hash(walk_arena): {:?}/call over 10 calls",
            start.elapsed() / 10
        );
    }
}
