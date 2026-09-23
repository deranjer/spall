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

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use glam::Vec3;
use glam::camera::rh;
use spall_core::{BUTTON_JUMP, GlobalCell, MaterialId};
use spall_physics::CharacterParams;
use spall_voxel::{Sample, Volume};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use crate::ClientError;
use crate::interactive::{InteractiveSession, InteractiveView, LiveInput};
use crate::net::{ClientNetConfig, run_replication_client};
use crate::predict::CELL_M;
use crate::replica::ReplicaWorld;

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
const REBUILD_DISTANCE_M: f64 = 1.0;
/// How often, at most, a *stationary* player re-requests a rebuild just to
/// notice a terrain edit landing nearby (a moving player already re-requests
/// every `REBUILD_DISTANCE_M`). This governs background-worker traffic, not
/// frame time — see [`RebuildWorker`] — so it only needs to be "responsive
/// enough for a person to notice", not "cheap".
const TERRAIN_RECHECK_INTERVAL: Duration = Duration::from_millis(500);

const MOUSE_SENSITIVITY: f32 = 0.0025;
const MAX_PITCH: f32 = 1.5;

/// Runs an interactively-played `sandbox-client`: connects `net_config` on a
/// background thread and opens a render window on the calling thread. Blocks
/// until the window closes or the network session ends on its own (the
/// server closing, a fatal transport error). `net_config.interactive` is set
/// by this function; any value the caller passed there is overwritten.
pub fn run_interactive_window(mut net_config: ClientNetConfig) -> Result<(), ClientError> {
    let session = InteractiveSession::new();
    net_config.interactive = Some(session.clone());

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
    let mut app = InteractiveApp::new(session.clone())?;

    let net_thread = std::thread::Builder::new()
        .name("spall-client-net".into())
        .spawn(move || {
            let result = run_replication_client(net_config);
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

struct RebuildOutcome {
    center_m: [f64; 3],
    instances: Vec<Instance>,
    elapsed: Duration,
}

impl RebuildWorker {
    /// Spawns the worker thread. It exits on its own once `request_tx`'s
    /// last sender (owned by the `InteractiveApp` this returns into) drops —
    /// no explicit shutdown signal or join needed.
    fn spawn(session: Arc<InteractiveSession>) -> Result<Self, ClientError> {
        let (request_tx, request_rx) = mpsc::channel::<[f64; 3]>();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("spall-client-rebuild".into())
            .spawn(move || {
                for center_m in request_rx {
                    let Some(replica) = session.replica.get() else {
                        continue;
                    };
                    let volume = replica
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .terrain_volume()
                        .cloned();
                    let Some(volume) = volume else { continue };
                    let start = Instant::now();
                    let instances = build_instances(&volume, center_m);
                    let elapsed = start.elapsed();
                    if result_tx
                        .send(RebuildOutcome {
                            center_m,
                            instances,
                            elapsed,
                        })
                        .is_err()
                    {
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
    /// Worst single-frame total render time seen since the last report —
    /// see `record_frame`. Reset to `0.0` each time `report` runs.
    max_frame_ms: f32,
    /// Worst single-frame terrain-instance-buffer upload time seen since the
    /// last report (frames without a fresh upload don't count — see
    /// `FrameTiming::buffer_upload_ms`). Reset to `0.0` each time `report`
    /// runs.
    max_buffer_upload_ms: f32,
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
        self.max_frame_ms = 0.0;
        self.max_buffer_upload_ms = 0.0;
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
            "{fps:.0} fps | frame {:.1} ms (avg) / {max_frame_ms:.1} ms (max) | buffer upload {max_buffer_upload_ms:.1} ms (max) | \
             rebuild {:.1} ms ({} instances) | server tick {server_tick} | \
             +{new_corrections} corrections ({new_idle} idle) (lifetime max {max_correction_m:.3} m idle {max_idle_correction_m:.3} m vert {max_vertical_correction_m:.3} m horiz {max_horizontal_correction_m:.3} m) | \
             +{new_unmatched} unmatched (lifetime max displacement {max_unmatched_displacement_m:.3} m) | \
             window cache: {new_window_sweeps} sweeps ({new_window_rebuilds} rebuilt), {new_terrain_fallbacks} terrain fallbacks",
            self.frame_ms_ema, self.last_rebuild_ms, self.last_rebuild_instances,
        )
    }
}

impl InteractiveApp {
    fn new(session: Arc<InteractiveSession>) -> Result<Self, ClientError> {
        let rebuild = RebuildWorker::spawn(session.clone())?;
        let body_worker = BodyWorker::spawn(session.clone())?;
        Ok(Self {
            session,
            window: None,
            renderer: None,
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
        match WorldRenderer::new(window.clone()) {
            Ok(renderer) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
                self.set_cursor_locked(true);
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
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
            } if !self.cursor_locked => {
                self.set_cursor_locked(true);
            }
            WindowEvent::KeyboardInput { event, .. } => {
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
                let mut combined = if self.show_terrain {
                    self.last_terrain_instances.clone()
                } else {
                    Vec::new()
                };
                // Drain the body worker the same way as the terrain one
                // above: never blocks, only the newest result matters.
                while let Ok(draws) = self.body_worker.result_rx.try_recv() {
                    self.last_body_draws = draws;
                }
                if self.show_capsule
                    && let Some(v) = view
                {
                    const CAPSULE_DEBUG_COLOR: [f32; 3] = [0.1, 1.0, 1.0]; // bright cyan
                    combined.extend(build_capsule_debug_instances(
                        v.predicted.position_m,
                        CAPSULE_DEBUG_COLOR,
                    ));
                }

                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                let outcome = match renderer.begin_frame(Some(&combined)) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
                };
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
                        match renderer.finish_frame(acquired, cam.as_ref(), &body_instances) {
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

fn build_instances(volume: &Volume, center_m: [f64; 3]) -> Vec<Instance> {
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
                    color: jittered_color(material_color(material), cell),
                    scale: [1.0; 3],
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
                    color: jittered_color(material_color(material), cell),
                    scale: [1.0; 3],
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
fn debug_line(a: Vec3, b: Vec3, color: [f32; 3]) -> impl Iterator<Item = Instance> {
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
        color,
        scale: std::array::from_fn(|component| dimensions[component] / CELL_M),
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
fn build_capsule_debug_instances(feet_m: [f64; 3], color: [f32; 3]) -> Vec<Instance> {
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
        instances.extend(debug_line(bottom[i], bottom[j], color)); // bottom ring
        instances.extend(debug_line(top[i], top[j], color)); // top ring
        instances.extend(debug_line(bottom[i], top[i], color)); // vertical edge
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

/// A small deterministic palette. This debug renderer intentionally does not
/// read the scene's real `MaterialManifest` albedo — it exists to prove live
/// input -> network -> predicted movement works end to end, not to preview
/// real material art. The playground's own materials (ids `10`-`20`,
/// `spall_sim::fixtures::playground_manifest`) are the one exception: they
/// were chosen specifically to make that scene visually varied for a
/// hands-on session through *this* renderer, so they get an explicit,
/// matching entry here instead of landing on an arbitrary colour via the
/// generic palette's modulo.
/// Peak brightness variation applied per voxel by [`jittered_color`].
const COLOR_JITTER: f32 = 0.08;

/// `base` scaled by a stable pseudo-random brightness in
/// `1 +/- COLOR_JITTER`, hashed from the cell's coordinates so a voxel keeps
/// the same shade every rebuild and frame. Lets individual voxels read as
/// distinct without changing what material they are.
fn jittered_color(base: [f32; 3], cell: GlobalCell) -> [f32; 3] {
    let mut h = (cell.x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (cell.y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (cell.z as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 32;
    h = h.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^= h >> 32;
    let unit = (h & 0xFFFF) as f32 / 65_535.0; // 0..=1
    let factor = 1.0 + (unit * 2.0 - 1.0) * COLOR_JITTER;
    base.map(|c| (c * factor).clamp(0.0, 1.0))
}

fn material_color(id: MaterialId) -> [f32; 3] {
    match id.0 {
        10 => [0.25, 0.55, 0.2],  // grass
        11 => [0.4, 0.28, 0.15],  // loam
        12 => [0.65, 0.25, 0.18], // brick
        13 => [0.82, 0.7, 0.45],  // sandstone
        14 => [0.35, 0.38, 0.42], // slate
        15 => [0.85, 0.15, 0.15], // debris red
        16 => [0.9, 0.5, 0.1],    // debris orange
        17 => [0.9, 0.85, 0.15],  // debris yellow
        18 => [0.2, 0.75, 0.3],   // debris green
        19 => [0.2, 0.4, 0.9],    // debris blue
        20 => [0.6, 0.25, 0.8],   // debris purple
        _ => {
            const PALETTE: [[f32; 3]; 6] = [
                [0.55, 0.55, 0.58],
                [0.45, 0.32, 0.20],
                [0.30, 0.55, 0.30],
                [0.60, 0.55, 0.35],
                [0.35, 0.35, 0.60],
                [0.60, 0.35, 0.35],
            ];
            PALETTE[id.0 as usize % PALETTE.len()]
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    offset: [f32; 3],
    color: [f32; 3],
    /// Per-axis multiplier of the shared `CELL_M` cube mesh.
    scale: [f32; 3],
    /// Unit quaternion `[x, y, z, w]` applied to the cube mesh and its
    /// normals about its own centre. Identity for terrain and debug strokes.
    rotation: [f32; 4],
}

const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    light_dir: [f32; 4],
}

const SHADER: &str = r#"
struct Globals {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;

struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
};
struct InstanceIn {
    @location(2) offset: vec3<f32>,
    @location(3) color: vec3<f32>,
    @location(4) scale: vec3<f32>,
    @location(5) rotation: vec4<f32>,
};
struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec3<f32>,
};

fn quat_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}

@vertex
fn vs_main(v: VertexIn, inst: InstanceIn) -> VertexOut {
    var out: VertexOut;
    let world_pos = quat_rotate(inst.rotation, v.position * inst.scale) + inst.offset;
    out.clip_position = globals.view_proj * vec4<f32>(world_pos, 1.0);
    out.normal = quat_rotate(inst.rotation, v.normal);
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let light_dir = normalize(globals.light_dir.xyz);
    let ndotl = max(dot(normalize(in.normal), -light_dir), 0.0);
    let ambient = 0.25;
    let lit = in.color * (ambient + (1.0 - ambient) * ndotl);
    return vec4<f32>(lit, 1.0);
}
"#;

/// One axis-aligned cube face: `normal`, plus tangent axes `u`/`v` chosen so
/// `u x v == normal` (outward CCW winding for every face from the same
/// corner-order rule).
struct Face {
    normal: [f32; 3],
    u: [f32; 3],
    v: [f32; 3],
}

const FACES: [Face; 6] = [
    Face {
        normal: [1.0, 0.0, 0.0],
        u: [0.0, 1.0, 0.0],
        v: [0.0, 0.0, 1.0],
    },
    Face {
        normal: [-1.0, 0.0, 0.0],
        u: [0.0, 0.0, 1.0],
        v: [0.0, 1.0, 0.0],
    },
    Face {
        normal: [0.0, 1.0, 0.0],
        u: [0.0, 0.0, 1.0],
        v: [1.0, 0.0, 0.0],
    },
    Face {
        normal: [0.0, -1.0, 0.0],
        u: [1.0, 0.0, 0.0],
        v: [0.0, 0.0, 1.0],
    },
    Face {
        normal: [0.0, 0.0, 1.0],
        u: [1.0, 0.0, 0.0],
        v: [0.0, 1.0, 0.0],
    },
    Face {
        normal: [0.0, 0.0, -1.0],
        u: [0.0, 1.0, 0.0],
        v: [1.0, 0.0, 0.0],
    },
];

fn cube_mesh() -> (Vec<Vertex>, Vec<u16>) {
    let h = CELL_M / 2.0;
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for face in FACES {
        let base = vertices.len() as u16;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let position = [
                face.normal[0] * h + face.u[0] * su * h + face.v[0] * sv * h,
                face.normal[1] * h + face.u[1] * su * h + face.v[1] * sv * h,
                face.normal[2] * h + face.u[2] * su * h + face.v[2] * sv * h,
            ];
            vertices.push(Vertex {
                position,
                normal: face.normal,
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

struct WorldRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    depth_view: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    index_count: u32,
    instance_buffer: Option<(wgpu::Buffer, u32)>,
    /// Bodies, re-posed every frame after the swapchain acquire (see
    /// `finish_frame`).
    body_buffer: Option<(wgpu::Buffer, u32)>,
    /// The last camera basis the window built (`InteractiveApp` computes eye
    /// position / look direction; this struct only knows the surface aspect
    /// ratio needed to finish the projection).
    aspect: f32,
}

/// One render frame's timing breakdown — see [`WorldRenderer::begin_frame`]/
/// [`WorldRenderer::finish_frame`] and `Hud::record_frame`. Added ENG-69
/// round 12: the averaged `frame_ms_ema`
/// the HUD already tracked hides short, occasional stalls; a video review of
/// an actual hands-on run showed a "hold, then jump" pattern (~60% of
/// consecutive frames nearly identical, interspersed with larger jumps) that
/// an average can't diagnose. This breaks a frame into the stages that can
/// plausibly eat a vsync interval's worth of time on their own.
struct FrameTiming {
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
        }
    }
}

/// A frame that has acquired its swapchain image, awaiting only the
/// camera-dependent draw ([`WorldRenderer::finish_frame`]) — see
/// [`WorldRenderer::begin_frame`].
struct AcquiredFrame {
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
enum AcquireOutcome {
    Ready(AcquiredFrame),
    Skipped(FrameTiming),
}

impl WorldRenderer {
    fn new(window: Arc<Window>) -> Result<Self, ClientError> {
        use wgpu::util::DeviceExt;

        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| ClientError::Gpu("no compatible GPU adapter".into()))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("spall-interactive-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
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
        let depth_view = create_depth_view(&device, surface_config.width, surface_config.height);
        let aspect = surface_config.width as f32 / surface_config.height.max(1) as f32;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spall-interactive-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-interactive-globals-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-interactive-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-interactive-globals-bind-group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("spall-interactive-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 12,
                    shader_location: 1,
                },
            ],
        };
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 12,
                    shader_location: 3,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 24,
                    shader_location: 4,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 36,
                    shader_location: 5,
                },
            ],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("spall-interactive-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout, instance_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let (vertices, indices) = cube_mesh();
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-interactive-cube-vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-interactive-cube-indices"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            depth_view,
            pipeline,
            globals_buffer,
            globals_bind_group,
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
            instance_buffer: None,
            body_buffer: None,
            aspect,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
        self.depth_view = create_depth_view(&self.device, size.width, size.height);
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
    fn begin_frame(
        &mut self,
        instances: Option<&Vec<Instance>>,
    ) -> Result<AcquireOutcome, ClientError> {
        use wgpu::util::DeviceExt as _;

        let frame_start = Instant::now();
        let mut buffer_upload_ms = None;
        let mut instance_count = None;
        if let Some(instances) = instances {
            let upload_start = Instant::now();
            let buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("spall-interactive-instances"),
                    contents: bytemuck::cast_slice(instances),
                    usage: wgpu::BufferUsages::VERTEX,
                });
            buffer_upload_ms = Some(upload_start.elapsed().as_secs_f32() * 1000.0);
            instance_count = Some(instances.len() as u32);
            self.instance_buffer = Some((buffer, instances.len() as u32));
        }

        let acquire_start = Instant::now();
        let surface_texture = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            Err(wgpu::SurfaceError::Timeout) => {
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            Err(error) => return Err(ClientError::Render(error.to_string())),
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
    fn finish_frame(
        &mut self,
        acquired: AcquiredFrame,
        cam: Option<&(Vec3, Vec3)>,
        bodies: &[Instance],
    ) -> Result<FrameTiming, ClientError> {
        use wgpu::util::DeviceExt as _;

        let AcquiredFrame {
            frame_start,
            surface_texture,
            view,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
        } = acquired;

        let clear_color = wgpu::Color {
            r: 0.45,
            g: 0.65,
            b: 0.85,
            a: 1.0,
        };

        if let Some((eye, look_dir)) = cam {
            // Targets the wgpu/DirectX NDC (`z in [0, 1]`, Y-up) — matches
            // `spall_render::camera::Camera`'s convention.
            let view_matrix = rh::view::look_to_mat4(*eye, *look_dir, Vec3::Y);
            let proj = rh::proj::directx::perspective(
                75f32.to_radians(),
                self.aspect.max(1e-4),
                0.05,
                300.0,
            );
            let view_proj = proj * view_matrix;
            let globals = Globals {
                view_proj: view_proj.to_cols_array_2d(),
                light_dir: [0.35, -0.8, 0.25, 0.0],
            };
            self.queue
                .write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
        }

        self.body_buffer = (!bodies.is_empty()).then(|| {
            (
                self.device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("spall-interactive-body-instances"),
                        contents: bytemuck::cast_slice(bodies),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
                bodies.len() as u32,
            )
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-interactive-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-interactive-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            if cam.is_some() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.globals_bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
                for (buffer, count) in [&self.instance_buffer, &self.body_buffer]
                    .into_iter()
                    .flatten()
                {
                    if *count > 0 {
                        pass.set_vertex_buffer(1, buffer.slice(..));
                        pass.draw_indexed(0..self.index_count, 0, 0..*count);
                    }
                }
            }
        }
        let submit_start = Instant::now();
        self.queue.submit([encoder.finish()]);
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
        let _ = self.device.poll(wgpu::Maintain::Wait);
        let submit_ms = submit_start.elapsed().as_secs_f32() * 1000.0;
        let present_start = Instant::now();
        surface_texture.present();
        let present_ms = present_start.elapsed().as_secs_f32() * 1000.0;
        Ok(FrameTiming {
            total_ms: frame_start.elapsed().as_secs_f32() * 1000.0,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
            submit_ms,
            present_ms,
        })
    }
}

fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("spall-interactive-depth"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
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
    fn capsule_debug_edges_are_thin_continuous_wire_strokes() {
        let lines = build_capsule_debug_instances([0.0; 3], [0.1, 1.0, 1.0]);
        assert_eq!(lines.len(), 12, "one cuboid per bounding-box edge");
        for line in lines {
            let dimensions = line.scale.map(|scale| scale * CELL_M);
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
            template: Arc::new(vec![Instance {
                offset: [0.0, 0.0, 0.0],
                color: [1.0, 0.0, 0.0],
                scale: [1.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
            }]),
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
