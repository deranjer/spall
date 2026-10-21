//! The local player's breathing voice: one recording per state, crossfaded as
//! endurance and exertion change. It owns its own worker and output stream, apart
//! from ambience and footsteps. A state whose recording is missing stays silent.

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU32, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use rodio::{Decoder, OutputStreamBuilder, Sink};

/// How the player is breathing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreathingState {
    /// At ease: silent or very soft.
    Calm,
    /// Sprinting, or not yet recovered: steady fast breathing.
    Exerted,
    /// Out of endurance: heavy panting.
    Exhausted,
}

impl BreathingState {
    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Exerted,
            2 => Self::Exhausted,
            _ => Self::Calm,
        }
    }
}

/// Endurance below this is still heavy breathing, sprinting or not.
pub const EXERTED_BELOW_ENDURANCE: f32 = 0.6;

/// Breathing from the character's endurance. Exhaustion lasts until the kernel
/// clears it (past its recovery threshold); recovery then eases through
/// [`BreathingState::Exerted`] back to calm.
pub fn select_state(endurance: f32, exhausted: bool, sprinting: bool) -> BreathingState {
    if exhausted {
        BreathingState::Exhausted
    } else if sprinting || endurance < EXERTED_BELOW_ENDURANCE {
        BreathingState::Exerted
    } else {
        BreathingState::Calm
    }
}

/// One state's recording.
#[derive(Debug, Clone)]
pub struct BreathingRow {
    pub state: BreathingState,
    /// `None` or a missing file: the state is silent.
    pub path: Option<PathBuf>,
    pub gain: f32,
    /// `true` loops the recording while the state lasts; `false` plays it once per
    /// entry into the state.
    pub looped: bool,
}

/// The game's breathing table: one row per state.
#[derive(Debug, Clone, Default)]
pub struct BreathingConfig {
    pub rows: Vec<BreathingRow>,
}

/// Time constant of a voice fading toward its target.
const FADE: Duration = Duration::from_millis(700);
const UPDATE: Duration = Duration::from_millis(25);

/// Sets the state a worker crossfades to.
#[derive(Clone)]
pub struct BreathingAudio {
    state: Arc<AtomicU8>,
    gain: Arc<AtomicU32>,
}

impl BreathingAudio {
    /// `None` when audio is off or no row has a recording to play.
    pub fn start(config: &BreathingConfig, gain: f32) -> Option<Self> {
        if std::env::var_os("SPALL_AUDIO").is_some_and(|value| value == "0")
            || !config.rows.iter().any(|row| row.path.is_some())
        {
            return None;
        }
        let state = Arc::new(AtomicU8::new(BreathingState::Calm.code()));
        let gain = Arc::new(AtomicU32::new(gain.clamp(0.0, 1.0).to_bits()));
        let (worker_state, worker_gain) = (Arc::clone(&state), Arc::clone(&gain));
        let rows = config.rows.clone();
        thread::Builder::new()
            .name("spall-breathing".into())
            .spawn(move || run_breathing(rows, worker_state, worker_gain))
            .ok()?;
        Some(Self { state, gain })
    }

    pub fn set_state(&self, state: BreathingState) {
        self.state.store(state.code(), Ordering::Relaxed);
    }

    pub fn set_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }
}

struct Voice {
    sink: Sink,
    volume: f32,
}

fn run_breathing(rows: Vec<BreathingRow>, state: Arc<AtomicU8>, gain: Arc<AtomicU32>) {
    let stream = match OutputStreamBuilder::open_default_stream() {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "breathing audio output unavailable; continuing without it");
            return;
        }
    };
    let mut voices: Vec<Option<Voice>> = rows.iter().map(|_| None).collect();
    // A recording that failed to load is reported once and left silent.
    let mut failed = vec![false; rows.len()];
    let mut last = Instant::now();
    loop {
        thread::sleep(UPDATE);
        // The owner dropped its handles: stop.
        if Arc::strong_count(&state) == 1 {
            break;
        }
        let current = BreathingState::from_code(state.load(Ordering::Relaxed));
        let master = f32::from_bits(gain.load(Ordering::Relaxed));
        let now = Instant::now();
        let dt = now.saturating_duration_since(last).as_secs_f32();
        last = now;
        let blend = 1.0 - (-dt / FADE.as_secs_f32()).exp();
        for (index, row) in rows.iter().enumerate() {
            let target = if row.state == current { 1.0 } else { 0.0 };
            if target > 0.0
                && voices[index].is_none()
                && !failed[index]
                && let Some(path) = &row.path
            {
                match load(stream.mixer(), path, row.looped) {
                    Ok(sink) => {
                        sink.play();
                        voices[index] = Some(Voice { sink, volume: 0.0 });
                    }
                    Err(error) => {
                        tracing::info!(state = ?row.state, %error, "breathing recording unavailable; silent");
                        failed[index] = true;
                    }
                }
            }
            let Some(voice) = &mut voices[index] else {
                continue;
            };
            voice.volume += (target - voice.volume) * blend;
            voice.sink.set_volume(voice.volume * row.gain * master);
            if target == 0.0 && voice.volume < 0.005 {
                voices[index] = None;
            }
        }
    }
    drop(voices);
    drop(stream);
}

/// A sink on `mixer` playing `path`, silent until the caller sets its volume.
fn load(mixer: &rodio::mixer::Mixer, path: &std::path::Path, looped: bool) -> Result<Sink, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let sink = Sink::connect_new(mixer);
    sink.set_volume(0.0);
    if looped {
        sink.append(Decoder::new_looped(file).map_err(|error| error.to_string())?);
    } else {
        sink.append(Decoder::try_from(file).map_err(|error| error.to_string())?);
    }
    Ok(sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breathing_follows_endurance_and_exertion() {
        use BreathingState::*;
        assert_eq!(select_state(1.0, false, false), Calm);
        assert_eq!(select_state(0.9, false, true), Exerted);
        assert_eq!(select_state(0.59, false, false), Exerted);
        assert_eq!(select_state(0.0, true, true), Exhausted);
        // Past the kernel's recovery threshold exhaustion is over, but breathing
        // stays heavy until endurance climbs back above the exerted line.
        assert_eq!(select_state(0.31, true, false), Exhausted);
        assert_eq!(select_state(0.31, false, false), Exerted);
        assert_eq!(select_state(0.61, false, false), Calm);
    }

    #[test]
    fn state_codes_round_trip() {
        for state in [
            BreathingState::Calm,
            BreathingState::Exerted,
            BreathingState::Exhausted,
        ] {
            assert_eq!(BreathingState::from_code(state.code()), state);
        }
    }
}
