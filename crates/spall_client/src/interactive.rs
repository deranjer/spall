//! Live keyboard/mouse-driven client session (T19 follow-up: interactive
//! window input).
//!
//! [`crate::net::run_replication_client`]'s mover normally reads a scripted
//! [`crate::net::MovementStep`] table. When a [`ClientNetConfig`](crate::net::ClientNetConfig)
//! carries an [`InteractiveSession`] instead, the mover reads [`LiveInput`]
//! every tick — written by the render window's key/mouse handlers on a
//! different thread — and publishes the predicted player's pose into
//! [`InteractiveSession::view`] every tick so the window can draw a live
//! camera. `None` (the default) leaves every existing scripted/headless run
//! byte-for-byte unchanged; this module is additive.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use spall_core::PlayerInput;
use spall_physics::CharacterState;

use crate::predict::CorrectionEvent;
use crate::replica::ReplicaWorld;

/// Thread-safe latest input state: written by the window's key/mouse event
/// handlers, read once per mover tick (~60 Hz). A plain mutex is cheap enough
/// at that rate and keeps `movement`/`view_dir`/`buttons` consistent with
/// each other (torn reads of independent atomics could see a movement axis
/// from one instant paired with a view direction from another).
#[derive(Debug)]
pub struct LiveInput(Mutex<PlayerInput>);

impl Default for LiveInput {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveInput {
    pub fn new() -> Self {
        Self(Mutex::new(PlayerInput::NEUTRAL))
    }

    /// The mover's per-tick read.
    pub fn snapshot(&self) -> PlayerInput {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The window's per-key-event write: local wish direction, each axis
    /// `-1.0..=1.0` (see [`PlayerInput`]).
    pub fn set_movement(&self, movement: [f32; 3]) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).movement = movement;
    }

    /// The window's per-mouse-motion write: world-space look direction.
    pub fn set_view_dir(&self, view_dir: [f32; 3]) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).view_dir = view_dir;
    }

    /// Sets or clears one held-button bit (see `spall_core::BUTTON_*`).
    pub fn set_button(&self, bit: u32, held: bool) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if held {
            guard.buttons |= bit;
        } else {
            guard.buttons &= !bit;
        }
    }
}

/// The local player's predicted pose, published once per mover tick so the
/// render window can place its camera without locking the predictor itself.
#[derive(Debug, Clone, Copy)]
pub struct InteractiveView {
    pub predicted: CharacterState,
    /// Last server tick observed when this pose was published.
    pub server_tick: u64,
    /// Wall-clock moment this pose was published. The mover ticks on its own
    /// ~60 Hz clock, independent of the render thread's own (also ~60 Hz,
    /// vsync-paced) redraw clock — two unsynchronized clocks at close to the
    /// same rate drift in and out of phase continuously, so a render frame
    /// landing right before a fresh mover tick repeats the previous pose,
    /// then "catches up" with a double-size jump on the frame that lands
    /// right after one. That beat pattern is visible as jitter even though
    /// the underlying motion is smooth. The window uses this timestamp to
    /// extrapolate the drawn position forward by `velocity * elapsed` instead
    /// of redrawing the exact same discrete pose on every frame between
    /// ticks.
    pub published_at: std::time::Instant,
    /// `PredictedPlayer::corrections` / `max_correction_m` as of this tick:
    /// how many times, and by how much (metres), a server snapshot has ever
    /// disagreed with what was predicted at the same input — see
    /// `PredictedPlayer::reconcile`. Surfaced so the window's HUD can show
    /// whether a moment of felt jerkiness lines up with a real reconciliation
    /// correction rather than something else (rendering, input, or just the
    /// character controller's own collision response).
    pub corrections: u64,
    pub max_correction_m: f64,
    /// `PredictedPlayer::idle_corrections` / `max_idle_correction_m` and the
    /// vertical/horizontal decomposition of `max_correction_m` — see those
    /// fields' docs. Split out so the HUD can tell a resting-contact
    /// disagreement (idle, vertical) from a collision-sweep one (moving,
    /// horizontal) without re-deriving it from raw logs.
    pub idle_corrections: u64,
    pub max_idle_correction_m: f64,
    pub max_vertical_correction_m: f64,
    pub max_horizontal_correction_m: f64,
}

/// Best-effort append-only JSONL log of every `CorrectionEvent`
/// `PredictedPlayer::reconcile` returns during an interactive session — for
/// post-hoc analysis of a *live, human-driven* run. Added ENG-69 round 10/11:
/// a CPU-only scripted trace (`crates/spall_client/tests/g1_ramp_trace.rs`)
/// could reproduce the seam mechanism but not the frequency/magnitude an
/// actual hands-on session showed (near-continuous corrections, a
/// substantial vertical component, a lifetime max that kept climbing past
/// what the ramp alone explained) — rather than write another synthetic
/// script and guess whether it covers the real gap (jumping? resting on a
/// slope? something the script never exercises?), this captures every real
/// event the real session produces, so the actual distribution can be
/// computed after the fact instead of inferred from a lifetime maximum.
pub struct CorrectionLog {
    file: Mutex<std::io::BufWriter<std::fs::File>>,
    started_at: std::time::Instant,
}

impl CorrectionLog {
    /// Creates (truncating any previous run's log) the file at `path`,
    /// including its parent directory.
    fn create(path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        Ok(Self {
            file: Mutex::new(std::io::BufWriter::new(file)),
            started_at: std::time::Instant::now(),
        })
    }

    /// Appends one event as a JSON line. Failures are swallowed — this log
    /// is a diagnostic aid, never a reason to disrupt the session itself.
    pub fn record(&self, server_tick: u64, event: CorrectionEvent) {
        use std::io::Write;
        let line = format!(
            "{{\"tick\":{server_tick},\"wall_ms\":{},\"error_m\":{:.6},\"vertical_m\":{:.6},\"horizontal_m\":{:.6},\"idle\":{}}}\n",
            self.started_at.elapsed().as_millis(),
            event.error_m,
            event.vertical_m,
            event.horizontal_m,
            event.idle,
        );
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

/// Shared handle between the network thread and the render window for one
/// interactively-played client session.
pub struct InteractiveSession {
    pub input: LiveInput,
    /// `None` until the local player's first authoritative snapshot arrives.
    pub view: Mutex<Option<InteractiveView>>,
    /// Set once by the network thread right after it builds the replica, so
    /// the window can read live terrain for its debug draw without owning
    /// (or racing) the mover's own lock acquisitions.
    pub replica: OnceLock<Arc<Mutex<ReplicaWorld>>>,
    /// The window sets this on close; the network thread's session-done
    /// select loop polls it so it disconnects promptly instead of relying on
    /// `overall_timeout`.
    pub stop: AtomicBool,
    /// `None` only if the log file could not be created (diagnostic, not
    /// required for the session to run) — see [`CorrectionLog`].
    pub corrections: Option<CorrectionLog>,
}

impl std::fmt::Debug for InteractiveSession {
    // `ClientNetConfig` derives `Debug`; a fixed placeholder is enough since
    // this session's live-updated fields (input, view, replica) don't carry
    // debuggable snapshots worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveSession").finish_non_exhaustive()
    }
}

/// Where `CorrectionLog` writes for a `cargo xtask play` session — fixed,
/// rather than plumbed through as a CLI flag, since this is diagnostic
/// infrastructure for one active investigation, not a permanent feature.
const CORRECTION_LOG_PATH: &str = ".local/runs/interactive-corrections.jsonl";

impl InteractiveSession {
    pub fn new() -> Arc<Self> {
        let corrections = match CorrectionLog::create(std::path::Path::new(CORRECTION_LOG_PATH)) {
            Ok(log) => Some(log),
            Err(e) => {
                eprintln!(
                    "spall-interactive: could not create {CORRECTION_LOG_PATH} ({e}); \
                     per-event correction logging is disabled for this session"
                );
                None
            }
        };
        Arc::new(Self {
            input: LiveInput::new(),
            view: Mutex::new(None),
            replica: OnceLock::new(),
            stop: AtomicBool::new(false),
            corrections,
        })
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
