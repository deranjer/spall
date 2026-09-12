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
}

impl std::fmt::Debug for InteractiveSession {
    // `ClientNetConfig` derives `Debug`; a fixed placeholder is enough since
    // this session's live-updated fields (input, view, replica) don't carry
    // debuggable snapshots worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveSession").finish_non_exhaustive()
    }
}

impl Default for InteractiveSession {
    fn default() -> Self {
        Self {
            input: LiveInput::new(),
            view: Mutex::new(None),
            replica: OnceLock::new(),
            stop: AtomicBool::new(false),
        }
    }
}

impl InteractiveSession {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
