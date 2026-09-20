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
use crate::interactive::{
    InteractiveSession, InteractiveView, LiveInput, ReviewCut, pick_review_lever,
    review_cut_request,
};
use crate::net::{ClientNetConfig, run_replication_client};
use crate::predict::CELL_M;

/// Debug-view visible radius (metres) around the player's feet. This
/// renderer walks the raw volume every rebuild (no meshing/culling beyond a
/// buried-cell filter), so a wide radius costs real per-rebuild time — but
/// that cost lands on the background `RebuildWorker` thread (see its own
/// doc), not the render/input thread, so a bigger radius only makes terrain
/// pop in a little later after a big camera jump, never stalls a frame.
/// `10.0` (the original value) left almost the whole scene invisible until
/// the player was standing right in front of it — raised after user report.
const VIEW_RADIUS_M: f32 = 96.0;
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

/// Reviewer (fly) camera: a much wider terrain draw around the camera, a fixed vertical
/// band (the scenes of interest sit within a few metres of the ground), and a coarser
/// rebuild step so flying does not chase the rebuild worker.
const FLY_VIEW_RADIUS_M: f32 = 160.0;
const FLY_CENTER_Y_M: f64 = 6.0;
const FLY_BAND_M: f32 = 9.0;
const FLY_REBUILD_DISTANCE_M: f64 = 20.0;
const FLY_SPEED_M_S: f32 = 10.0;
const FLY_FAST_FACTOR: f32 = 5.0;

/// What the background worker should build: where, how wide, and how tall a band.
#[derive(Clone, Copy)]
struct RebuildRequest {
    center_m: [f64; 3],
    radius_m: f32,
    up_m: f32,
    down_m: f32,
}

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

/// Reviewer tools: a free-fly camera (no physics, no collision, no server input) and
/// keybinds that kick off the wake-locality demo edits. Client-side only; the edits are
/// ordinary server-validated `Cut` actions.
///
/// Keys: `F` toggles fly mode; in fly mode `WASD` move, `Space` up, `Ctrl` down, `Shift` x5.
/// `1` cut the beam's left end (the counterweight; the beam must tip), `2` cut its right
/// end, `3` nibble its far tip (a harmless edit), `-`/`=` change the cut radius, `H` prints
/// this help.
struct ReviewCam {
    fly: bool,
    pos: [f64; 3],
    up: bool,
    down: bool,
    fast: bool,
    cut_radius_cells: i64,
    seq: u64,
}

impl Default for ReviewCam {
    fn default() -> Self {
        Self {
            fly: false,
            pos: [0.0, 10.0, 0.0],
            up: false,
            down: false,
            fast: false,
            cut_radius_cells: 6,
            seq: 0,
        }
    }
}

const REVIEW_HELP: &str = "reviewer keys: F fly camera (WASD move, Space up, Ctrl down, Shift fast) | \
1 cut the beam's LEFT end (must tip) | 2 cut RIGHT end | 3 nibble the far tip (harmless) | \
- / = cut radius | H help";
/// Cached visible cells of one body: `(volume revision, [(local centre, colour)])`.
type BodyCells = (u64, Vec<([f32; 3], [f32; 3])>);

struct InteractiveApp {
    session: Arc<InteractiveSession>,
    window: Option<Arc<Window>>,
    renderer: Option<WorldRenderer>,
    held: HeldKeys,
    yaw: f32,
    pitch: f32,
    cursor_locked: bool,
    rebuild: RebuildWorker,
    /// Centre the *last completed* background rebuild was built around — not
    /// the centre of the most recent request, which may still be in flight.
    last_built_pos: Option<[f64; 3]>,
    last_dispatch_at: Option<Instant>,
    /// The camera's own displayed feet position — deliberately a separate,
    /// smoothed value rather than reading `InteractiveView::predicted`
    /// directly every frame. See [`smoothed_eye`].
    display_feet: Option<([f64; 3], Instant)>,
    hud: Hud,
    /// Reviewer camera state (see [`ReviewCam`]).
    review: ReviewCam,
    /// Terrain draw list from the last completed rebuild (bodies are appended per frame).
    terrain_instances: Vec<Instance>,
    /// Local-cell splats per body, keyed by entity id and volume revision.
    body_cache: std::collections::HashMap<u64, BodyCells>,
    /// Hash of every drawn body's pose and revision at the last combined upload.
    last_bodies_sig: u64,
    last_frame_at: Option<Instant>,
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
    request_tx: mpsc::Sender<RebuildRequest>,
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
        let (request_tx, request_rx) = mpsc::channel::<RebuildRequest>();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("spall-client-rebuild".into())
            .spawn(move || {
                for request in request_rx {
                    let center_m = request.center_m;
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
                    let instances = build_instances_with(&volume, request);
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
            last_built_pos: None,
            last_dispatch_at: None,
            display_feet: None,
            hud: Hud::default(),
            review: ReviewCam::default(),
            terrain_instances: Vec::new(),
            body_cache: std::collections::HashMap::new(),
            last_bodies_sig: 0,
            last_frame_at: None,
            result: Ok(()),
        })
    }

    fn view_dir(&self) -> [f32; 3] {
        view_dir_from(self.yaw, self.pitch)
    }

    fn publish_look(&self) {
        self.session.input.set_view_dir(self.view_dir());
    }

    /// Splat instances for every replicated detached body near `cam`, plus a signature
    /// of the drawn state (pose bits and volume revision per body). Each occupied
    /// cell is an axis-aligned cube placed at the *rotated* cell centre — enough to
    /// see a body's real voxels move and tip; cubes do not themselves rotate.
    fn body_splats(&mut self, cam: [f64; 3]) -> (Vec<Instance>, u64) {
        const MAX_BODY_INSTANCES: usize = 400_000;
        const BODY_CULL_M: f64 = 220.0;
        let mut out = Vec::new();
        let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: u64| {
            sig ^= v;
            sig = sig.wrapping_mul(0x0100_0000_01b3);
        };
        let Some(replica) = self.session.replica.get() else {
            return (out, 0);
        };
        let g = replica.lock().unwrap_or_else(|e| e.into_inner());
        let cm = f64::from(CELL_M);
        // In the small review scene (a beam on a base) pick the beam out by its cell count.
        let counts: Vec<_> = g
            .body_ids()
            .map(|e| (e, g.body_solid_cells(e).unwrap_or(0)))
            .collect();
        let beam = if counts.len() <= 4 {
            pick_review_lever(&counts)
        } else {
            None
        };
        for (entity, volume_id) in g.body_volumes() {
            let Some(volume) = g.volume(volume_id) else {
                continue;
            };
            let tick = g.latest_motion_tick(entity).unwrap_or(0) as f64;
            let Some(pose) = g.interpolated_pose(entity, tick) else {
                continue;
            };
            let t = pose.translation_m;
            let dist2 = (t[0] - cam[0]).powi(2) + (t[1] - cam[1]).powi(2) + (t[2] - cam[2]).powi(2);
            if dist2 > BODY_CULL_M * BODY_CULL_M {
                continue;
            }
            let Ok(q) = pose.rotation.to_unit() else {
                continue;
            };
            let rev = volume.next_revision().0;
            mix(entity.get());
            mix(rev);
            for v in t {
                mix(v.to_bits());
            }
            for v in q {
                mix(u64::from(v.to_bits()));
            }
            let cells = self
                .body_cache
                .entry(entity.get())
                .or_insert_with(|| (u64::MAX, Vec::new()));
            if cells.0 != rev {
                cells.0 = rev;
                let color = BODY_PALETTE[(entity.get() as usize) % BODY_PALETTE.len()];
                let foot = (beam == Some(entity)).then_some(FOOT_COLOR);
                cells.1 = body_local_cells(volume, cm, color, foot);
            }
            if out.len() + cells.1.len() > MAX_BODY_INSTANCES {
                continue;
            }
            let rot = glam::DQuat::from_xyzw(
                f64::from(q[0]),
                f64::from(q[1]),
                f64::from(q[2]),
                f64::from(q[3]),
            );
            let origin = glam::DVec3::from_array(t);
            for (local, color) in &cells.1 {
                let w = origin
                    + rot
                        * glam::DVec3::new(
                            f64::from(local[0]),
                            f64::from(local[1]),
                            f64::from(local[2]),
                        );
                out.push(Instance {
                    offset: [w.x as f32, w.y as f32, w.z as f32],
                    color: *color,
                });
            }
        }
        (out, sig)
    }

    /// Queues a review cut on the beam (see [`ReviewCam`]): a server-validated `Cut`
    /// aimed from 3 m in front of the chosen end, so the ray hits the beam's front face
    /// wherever the fly camera happens to be.
    fn review_cut(&mut self, cut: ReviewCut) {
        let Some(replica) = self.session.replica.get() else {
            eprintln!("spall-review: not connected yet");
            return;
        };
        let picked = {
            let g = replica.lock().unwrap_or_else(|e| e.into_inner());
            let counts: Vec<_> = g
                .body_ids()
                .map(|e| (e, g.body_solid_cells(e).unwrap_or(0)))
                .collect();
            pick_review_lever(&counts).and_then(|e| {
                let tick = g.latest_motion_tick(e).unwrap_or(0) as f64;
                let pose = g.interpolated_pose(e, tick)?;
                Some((e, pose.translation_m, pose.rotation.to_unit().ok()?))
            })
        };
        let Some((entity, t, q)) = picked else {
            eprintln!("spall-review: no bodies here — start with `--scene review-lever`");
            return;
        };
        self.review.seq += 1;
        let seq = self.review.seq;
        let Some(request) =
            review_cut_request(entity, t, q, cut, self.review.cut_radius_cells, seq)
        else {
            return;
        };
        self.session.push_action(request);
        let (cx, cy, radius_override) = cut.target();
        let radius = radius_override
            .unwrap_or(self.review.cut_radius_cells)
            .clamp(1, 8);
        eprintln!(
            "spall-review: cut #{seq} sent (entity {}, local cell ({cx},{cy}), radius {radius} cells)",
            entity.get()
        );
    }

    /// Toggles the fly camera, starting from wherever the player's eye is.
    fn toggle_fly(&mut self) {
        self.review.fly = !self.review.fly;
        if self.review.fly {
            let eye = self.display_feet.map(|(f, _)| f).unwrap_or([0.0, 1.0, 0.0]);
            self.review.pos = [eye[0], eye[1] + 3.0, eye[2] + 2.0];
        }
        self.last_built_pos = None; // rebuild around the new centre
        self.publish_movement();
        eprintln!(
            "spall-review: fly camera {} — {REVIEW_HELP}",
            if self.review.fly {
                "ON (no physics, no server input)"
            } else {
                "off"
            }
        );
    }

    fn publish_movement(&self) {
        // The reviewer camera never drives the player.
        let movement = if self.review.fly {
            [0.0; 3]
        } else {
            self.held.movement()
        };
        self.session.input.set_movement(movement);
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
                        if self.review.fly {
                            self.review.up = held;
                        } else {
                            self.held.jump = held;
                            self.session.input.set_button(BUTTON_JUMP, held);
                        }
                    }
                    KeyCode::ControlLeft => self.review.down = held,
                    KeyCode::ShiftLeft => self.review.fast = held,
                    KeyCode::KeyF if held && !event.repeat => self.toggle_fly(),
                    KeyCode::KeyH if held && !event.repeat => {
                        eprintln!("spall-review: {REVIEW_HELP}")
                    }
                    KeyCode::Digit1 if held && !event.repeat => self.review_cut(ReviewCut::LeftEnd),
                    KeyCode::Digit2 if held && !event.repeat => {
                        self.review_cut(ReviewCut::RightEnd)
                    }
                    KeyCode::Digit3 if held && !event.repeat => self.review_cut(ReviewCut::FarTip),
                    KeyCode::Equal if held && !event.repeat => {
                        self.review.cut_radius_cells = (self.review.cut_radius_cells + 1).min(8);
                        eprintln!(
                            "spall-review: cut radius {} cells",
                            self.review.cut_radius_cells
                        );
                    }
                    KeyCode::Minus if held && !event.repeat => {
                        self.review.cut_radius_cells = (self.review.cut_radius_cells - 1).max(1);
                        eprintln!(
                            "spall-review: cut radius {} cells",
                            self.review.cut_radius_cells
                        );
                    }
                    KeyCode::Escape if held && !event.repeat => {
                        self.set_cursor_locked(false);
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
                // Fly-camera movement (reviewer mode): free of physics, collision and
                // the server.
                let dt = self
                    .last_frame_at
                    .map_or(0.0, |t| (now - t).as_secs_f32())
                    .min(0.1);
                self.last_frame_at = Some(now);
                if self.review.fly {
                    let fwd = Vec3::from_array(view_dir_from(self.yaw, self.pitch));
                    let right = Vec3::new(self.yaw.cos(), 0.0, self.yaw.sin());
                    let mut v = Vec3::ZERO;
                    if self.held.forward {
                        v += fwd;
                    }
                    if self.held.back {
                        v -= fwd;
                    }
                    if self.held.right {
                        v += right;
                    }
                    if self.held.left {
                        v -= right;
                    }
                    if self.review.up {
                        v += Vec3::Y;
                    }
                    if self.review.down {
                        v -= Vec3::Y;
                    }
                    let speed = FLY_SPEED_M_S
                        * if self.review.fast {
                            FLY_FAST_FACTOR
                        } else {
                            1.0
                        };
                    let step = v * speed * dt;
                    for (a, s) in [step.x, step.y, step.z].into_iter().enumerate() {
                        self.review.pos[a] += f64::from(s);
                    }
                }

                // Drain the background worker's result, if a fresh one has
                // landed since the last frame (never blocks — `try_recv`).
                // Only the newest matters if somehow more than one queued up.
                let mut new_terrain = false;
                while let Ok(outcome) = self.rebuild.result_rx.try_recv() {
                    self.hud
                        .record_rebuild(outcome.elapsed, outcome.instances.len());
                    self.last_built_pos = Some(outcome.center_m);
                    self.terrain_instances = outcome.instances;
                    new_terrain = true;
                    self.rebuild.in_flight = false;
                }

                let view = *self.session.view.lock().unwrap_or_else(|e| e.into_inner());

                let fly = self.review.fly;
                let request = if fly {
                    Some(RebuildRequest {
                        center_m: [self.review.pos[0], FLY_CENTER_Y_M, self.review.pos[2]],
                        radius_m: FLY_VIEW_RADIUS_M,
                        up_m: FLY_BAND_M,
                        down_m: FLY_BAND_M,
                    })
                } else {
                    view.map(|v| RebuildRequest {
                        center_m: v.predicted.position_m,
                        radius_m: VIEW_RADIUS_M,
                        up_m: VIEW_HEIGHT_UP_M,
                        down_m: VIEW_HEIGHT_DOWN_M,
                    })
                };
                if let Some(request) = request {
                    let c = request.center_m;
                    let step = if fly {
                        FLY_REBUILD_DISTANCE_M
                    } else {
                        REBUILD_DISTANCE_M
                    };
                    let moved_far_enough = self.last_built_pos.is_none_or(|p| {
                        let d = [c[0] - p[0], c[1] - p[1], c[2] - p[2]];
                        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() >= step
                    });
                    let due_for_recheck = self
                        .last_dispatch_at
                        .is_none_or(|t| now - t >= TERRAIN_RECHECK_INTERVAL);
                    // At most one request in flight — a faster player than the
                    // worker can keep up with just rides on a slightly stale
                    // draw rather than queuing requests it'll never need.
                    if !self.rebuild.in_flight
                        && (moved_far_enough || due_for_recheck)
                        && self.rebuild.request_tx.send(request).is_ok()
                    {
                        self.rebuild.in_flight = true;
                        self.last_dispatch_at = Some(now);
                    }
                }

                // Detached bodies: drawn as rotated cell splats from the replica's body
                // volumes and interpolated poses, re-uploaded only when a pose or a
                // volume revision changes (or the terrain list is replaced).
                let cam_pos = if fly {
                    self.review.pos
                } else {
                    view.map_or([0.0; 3], |v| v.predicted.position_m)
                };
                let (body_instances, sig) = self.body_splats(cam_pos);
                let combined = if new_terrain || sig != self.last_bodies_sig {
                    self.last_bodies_sig = sig;
                    let mut all =
                        Vec::with_capacity(self.terrain_instances.len() + body_instances.len());
                    all.extend_from_slice(&self.terrain_instances);
                    all.extend(body_instances);
                    Some(all)
                } else {
                    None
                };

                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                let outcome = match renderer.begin_frame(combined.as_ref()) {
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
                        let cam = if self.review.fly {
                            let p = self.review.pos;
                            Some((Vec3::new(p[0] as f32, p[1] as f32, p[2] as f32), look_dir))
                        } else {
                            fresh_view.map(|v| {
                                (
                                    compute_smoothed_eye(&mut self.display_feet, v, render_now),
                                    look_dir,
                                )
                            })
                        };
                        match renderer.finish_frame(acquired, cam.as_ref()) {
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

/// The eye position for the current predicted pose: feet, raised to (90% of)
/// standing eye height. Cheap — no lock, no volume access — so it stays
/// directly on the render/input thread; only the terrain draw list
/// ([`RebuildWorker`]) is expensive enough to need moving off of it.
/// A render frame lands on its own (vsync-paced) clock, independent of the
/// mover's own ~60 Hz tick clock — see [`InteractiveView::published_at`] — so
/// extrapolate the feet forward by the time elapsed since that tick was
/// published, using the velocity it reported, rather than redrawing the
/// exact same discrete pose on every frame between ticks (the jitter that
/// produces is a beat pattern between the two unsynchronized clocks, not
/// anything wrong with the underlying motion). Clamped short in case the
/// mover has stalled (a lost connection, a debugger break) — extrapolating
/// indefinitely would fling the camera off in whatever direction it was last
/// moving.
const MAX_EXTRAPOLATION_S: f32 = 0.1;

/// How quickly the *displayed* camera position catches up to the raw
/// (extrapolated) predicted one — see [`compute_smoothed_eye`].
/// Short enough to add well under a frame's worth of lag to genuinely
/// continuous movement (WASD keeps moving the target every frame, so
/// smoothing barely touches it), long enough to turn a `PredictedPlayer`
/// reconciliation correction — measured live at a small, consistent ~0.15 m,
/// happening even while standing still (resting-contact micro-jitter
/// between the client's and server's independently-computed physics, not a
/// bug introduced by this window) — into a brief, barely-visible glide
/// instead of a snap.
const CORRECTION_SMOOTHING_TAU_S: f32 = 0.05;

/// The raw predicted feet position, extrapolated forward by the time elapsed
/// since the mover published it — not yet smoothed for a correction (see
/// [`compute_smoothed_eye`], which is what callers actually want).
fn extrapolated_feet(view: InteractiveView) -> [f64; 3] {
    let dt = view
        .published_at
        .elapsed()
        .as_secs_f32()
        .min(MAX_EXTRAPOLATION_S);
    let feet = view.predicted.position_m;
    let v = view.predicted.velocity_m_s;
    [
        feet[0] + f64::from(v[0] * dt),
        feet[1] + f64::from(v[1] * dt),
        feet[2] + f64::from(v[2] * dt),
    ]
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

/// The eye position to render this frame: [`extrapolated_feet`], smoothed
/// ([`CORRECTION_SMOOTHING_TAU_S`]) against `*display_feet` — the camera's
/// own previous displayed position — rather than the target position used
/// outright. See that constant's doc for why: this exists to turn a
/// reconciliation correction into a glide instead of a snap, without adding
/// meaningfully more lag to intentional movement.
///
/// A free function taking `display_feet` directly (not an `InteractiveApp`
/// method) for the same borrow-checker reason as [`view_dir_from`]: the
/// `RedrawRequested` handler calls this after `WorldRenderer::begin_frame`,
/// while `self.renderer` is still mutably borrowed for the matching
/// `finish_frame` call — see ENG-69 round 13's doc on `begin_frame`.
fn compute_smoothed_eye(
    display_feet: &mut Option<([f64; 3], Instant)>,
    view: InteractiveView,
    now: Instant,
) -> Vec3 {
    let target = extrapolated_feet(view);
    let feet = match *display_feet {
        Some((prev, prev_at)) => {
            let dt = (now - prev_at).as_secs_f32().max(0.0);
            let factor = 1.0 - (-dt / CORRECTION_SMOOTHING_TAU_S).exp();
            [
                prev[0] + (target[0] - prev[0]) * f64::from(factor),
                prev[1] + (target[1] - prev[1]) * f64::from(factor),
                prev[2] + (target[2] - prev[2]) * f64::from(factor),
            ]
        }
        None => target,
    };
    *display_feet = Some((feet, now));
    let eye_height_m = f64::from(CharacterParams::DEFAULT.total_height_m()) * 0.9;
    Vec3::new(
        feet[0] as f32,
        (feet[1] + eye_height_m) as f32,
        feet[2] as f32,
    )
}

#[cfg(test)]
fn build_instances(volume: &Volume, center_m: [f64; 3]) -> Vec<Instance> {
    build_instances_with(
        volume,
        RebuildRequest {
            center_m,
            radius_m: VIEW_RADIUS_M,
            up_m: VIEW_HEIGHT_UP_M,
            down_m: VIEW_HEIGHT_DOWN_M,
        },
    )
}

fn build_instances_with(volume: &Volume, request: RebuildRequest) -> Vec<Instance> {
    let center_m = request.center_m;
    let cell_m = f64::from(CELL_M);
    let center_cell = GlobalCell::new(
        (center_m[0] / cell_m).floor() as i64,
        (center_m[1] / cell_m).floor() as i64,
        (center_m[2] / cell_m).floor() as i64,
    );
    let horiz = (request.radius_m / CELL_M).ceil() as i64;
    let up = (request.up_m / CELL_M).ceil() as i64;
    let down = (request.down_m / CELL_M).ceil() as i64;

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
                    color: material_color(material),
                });
            }
        }
    }
    instances
}

/// A body volume's visible cells as `(local centre in the body frame, colour)`; cells
/// buried inside solid matter are skipped, like the terrain draw.
fn body_local_cells(
    volume: &Volume,
    cell_m: f64,
    body_color: [f32; 3],
    foot_color: Option<[f32; 3]>,
) -> Vec<([f32; 3], [f32; 3])> {
    let mut out = Vec::new();
    for coord in volume.resident_brick_coords() {
        for lz in 0..32i64 {
            for ly in 0..32i64 {
                for lx in 0..32i64 {
                    let cell =
                        GlobalCell::new(coord.x * 32 + lx, coord.y * 32 + ly, coord.z * 32 + lz);
                    let Ok(Sample::Filled(material)) = volume.sample(cell) else {
                        continue;
                    };
                    if is_buried(volume, cell) {
                        continue;
                    }
                    out.push((
                        [
                            ((cell.x as f64 + 0.5) * cell_m) as f32,
                            ((cell.y as f64 + 0.5) * cell_m) as f32,
                            ((cell.z as f64 + 0.5) * cell_m) as f32,
                        ],
                        // Each body gets its own colour (terrain keeps the material palette)
                        // so separate bodies read apart; the review beam's foot is picked out.
                        match foot_color {
                            Some(foot) if cell.y < 4 => foot,
                            _ => {
                                let _ = material;
                                body_color
                            }
                        },
                    ));
                }
            }
        }
    }
    out
}

/// Distinct, saturated colours for detached bodies, by entity id (index 1 = blue, 2 = orange:
/// the review scene's base and beam).
const BODY_PALETTE: [[f32; 3]; 6] = [
    [0.35, 0.85, 0.40],
    [0.25, 0.55, 0.95],
    [0.95, 0.55, 0.15],
    [0.90, 0.30, 0.70],
    [0.95, 0.85, 0.20],
    [0.30, 0.85, 0.85],
];
/// The review beam's foot (its local cells below y = 4): the pivot it must balance on.
const FOOT_COLOR: [f32; 3] = [0.95, 0.20, 0.20];

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
/// real material art.
fn material_color(id: MaterialId) -> [f32; 3] {
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
}

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
};
struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec3<f32>,
};

@vertex
fn vs_main(v: VertexIn, inst: InstanceIn) -> VertexOut {
    var out: VertexOut;
    let world_pos = v.position + inst.offset;
    out.clip_position = globals.view_proj * vec4<f32>(world_pos, 1.0);
    out.normal = v.normal;
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
    ) -> Result<FrameTiming, ClientError> {
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
            if cam.is_some()
                && let Some((instance_buffer, count)) = &self.instance_buffer
                && *count > 0
            {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.globals_bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.set_vertex_buffer(1, instance_buffer.slice(..));
                pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
                pass.draw_indexed(0..self.index_count, 0, 0..*count);
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

    use super::{HeldKeys, LiveInput};

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
