//! Best-effort client ambience. Device ownership and file decoding stay off
//! the render thread; the game supplies its own biome-indexed asset paths.

use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use rodio::{Decoder, OutputStreamBuilder, Sink};

const CROSSFADE: Duration = Duration::from_millis(900);
const UPDATE: Duration = Duration::from_millis(25);

/// Game-owned Ogg paths, indexed by the stable biome code in world generation.
#[derive(Debug, Clone, Default)]
pub struct BiomeAudioConfig {
    pub tracks: Vec<Option<PathBuf>>,
    pub labels: Vec<String>,
    /// Weather beds indexed by the stable WeatherKind code; game-owned assets.
    pub weather_tracks: Vec<Option<PathBuf>>,
    pub thunder: Option<PathBuf>,
}

/// Resolves a world-space listener position into the replicated climate field.
/// Bounds are checked before `CoverGrid::at`, whose public lookup otherwise
/// clamps missing coordinates to an edge biome.
pub fn biome_at(
    field: &spall_ecology::cover::CoverField,
    grid: &spall_ecology::cover::CoverGrid,
    position_m: [f64; 3],
) -> Option<u8> {
    if field.is_empty()
        || grid.width() == 0
        || !position_m[0].is_finite()
        || !position_m[2].is_finite()
    {
        return None;
    }
    let cell_m = f64::from(crate::predict::CELL_M);
    let x = (position_m[0] / cell_m).floor();
    let z = (position_m[2] / cell_m).floor();
    if x < 0.0 || z < 0.0 || x > i64::MAX as f64 || z > i64::MAX as f64 {
        return None;
    }
    let (x, z) = (x as i64, z as i64);
    let edge = i64::try_from(grid.width())
        .ok()?
        .checked_mul(spall_ecology::cover::FIELD_CELLS)?;
    if x >= edge || z >= edge {
        return None;
    }
    Some(grid.at(x, z).0 as u8)
}

impl BiomeAudioConfig {
    pub fn track(&self, biome: u8) -> Option<&PathBuf> {
        self.tracks.get(usize::from(biome)).and_then(Option::as_ref)
    }
}

/// Checks the shipped loop format and decodes initial samples without opening
/// an output device. Full originals remain preserved in the research archive.
pub fn validate_asset(path: &std::path::Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut source = Decoder::new_looped(file).map_err(|error| error.to_string())?;
    for _ in 0..4096 {
        if source.next().is_none() {
            return Err("loop decoder produced no samples".into());
        }
    }
    Ok(())
}

/// Exercises one finite pass and a subsequent loop pass, for compact boundary
/// regression fixtures such as the short alpine call.
pub fn validate_loop_boundary(path: &std::path::Path) -> Result<(), String> {
    let samples = Decoder::try_from(std::fs::File::open(path).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?
        .count();
    if samples == 0 {
        return Err("decoder produced no samples".into());
    }
    let looped = Decoder::new_looped(std::fs::File::open(path).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    if looped.take(samples + 4096).count() != samples + 4096 {
        return Err("loop decoder ended at the clip boundary".into());
    }
    Ok(())
}

fn existing_slot(paths: &[Option<PathBuf>; 2], request: &Option<PathBuf>) -> Option<usize> {
    request.as_ref().and_then(|path| {
        paths
            .iter()
            .position(|stored| stored.as_ref() == Some(path))
    })
}

/// Sends desired tracks to a worker that owns the audio device and both sinks.
#[derive(Clone)]
pub struct AmbienceAudio {
    tx: SyncSender<Option<PathBuf>>,
    one_shots: SyncSender<PathBuf>,
    gain: Arc<AtomicU32>,
}

impl AmbienceAudio {
    pub fn start(gain: f32) -> Option<Self> {
        if std::env::var_os("SPALL_AUDIO").is_some_and(|value| value == "0") {
            return None;
        }
        let gain = std::env::var("SPALL_AUDIO_VOLUME")
            .ok()
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| value.is_finite())
            .map_or(gain, |value| value.clamp(0.0, 1.0));
        let (tx, rx) = mpsc::sync_channel(1);
        let (one_shots, shot_rx) = mpsc::sync_channel(4);
        let gain = Arc::new(AtomicU32::new(gain.to_bits()));
        let worker_gain = Arc::clone(&gain);
        thread::Builder::new()
            .name("spall-ambience".into())
            .spawn(move || run_audio(rx, shot_rx, worker_gain))
            .ok()?;
        Some(Self {
            tx,
            one_shots,
            gain,
        })
    }

    pub fn set_track(&self, path: Option<PathBuf>) -> Result<(), TrySendError<Option<PathBuf>>> {
        match self.tx.try_send(path) {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(safe_gain(gain).to_bits(), Ordering::Relaxed);
    }

    pub fn play_once(&self, path: PathBuf) -> Result<(), TrySendError<PathBuf>> {
        self.one_shots.try_send(path)
    }
}

fn safe_gain(gain: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn run_audio(
    rx: mpsc::Receiver<Option<PathBuf>>,
    shot_rx: mpsc::Receiver<PathBuf>,
    gain: Arc<AtomicU32>,
) {
    let stream = match OutputStreamBuilder::open_default_stream() {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "ambience audio output unavailable; continuing without audio");
            return;
        }
    };
    let sinks = [
        Sink::connect_new(stream.mixer()),
        Sink::connect_new(stream.mixer()),
    ];
    let thunder = Sink::connect_new(stream.mixer());
    let mut paths: [Option<PathBuf>; 2] = [None, None];
    let mut volumes = [0.0_f32; 2];
    let mut fade = Crossfade {
        from: [0.0; 2],
        to: [0.0; 2],
    };
    let mut fade_started = Instant::now();

    loop {
        match rx.recv_timeout(UPDATE) {
            Ok(request) => {
                if let Some(index) = existing_slot(&paths, &request) {
                    let mut to = [0.0, 0.0];
                    to[index] = 1.0;
                    fade = Crossfade { from: volumes, to };
                    fade_started = Instant::now();
                } else if let Some(path) = request {
                    let index = if volumes[0] <= volumes[1] { 0 } else { 1 };
                    match std::fs::File::open(&path)
                        .map_err(|error| error.to_string())
                        .and_then(|file| {
                            Decoder::new_looped(file).map_err(|error| error.to_string())
                        }) {
                        Ok(source) => {
                            sinks[index].clear();
                            // The inactive slot may retain part of a previous
                            // rapid fade. It has been cut above, so reflect zero
                            // in the new transition's starting level.
                            volumes[index] = 0.0;
                            sinks[index].set_volume(0.0);
                            sinks[index].append(source);
                            sinks[index].play();
                            paths[index] = Some(path);
                            let mut to = [0.0, 0.0];
                            to[index] = 1.0;
                            fade = Crossfade { from: volumes, to };
                            fade_started = Instant::now();
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not load biome ambience; fading to silence");
                            fade = Crossfade {
                                from: volumes,
                                to: [0.0, 0.0],
                            };
                            fade_started = Instant::now();
                        }
                    }
                } else {
                    fade = Crossfade {
                        from: volumes,
                        to: [0.0, 0.0],
                    };
                    fade_started = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // One bounded one-shot voice. Do not build an unbounded thunder queue.
        while let Ok(path) = shot_rx.try_recv() {
            if thunder.empty() {
                match std::fs::File::open(path)
                    .map_err(|e| e.to_string())
                    .and_then(|f| Decoder::try_from(f).map_err(|e| e.to_string()))
                {
                    Ok(source) => {
                        thunder.append(source);
                        thunder.play();
                    }
                    Err(error) => tracing::warn!(%error, "weather one-shot unavailable"),
                }
            }
        }
        let gain = f32::from_bits(gain.load(Ordering::Relaxed));
        thunder.set_volume(gain);
        let t = (fade_started.elapsed().as_secs_f32() / CROSSFADE.as_secs_f32()).clamp(0.0, 1.0);
        volumes = fade.sample(t);
        for index in 0..2 {
            sinks[index].set_volume(volumes[index] * gain);
            if t >= 1.0 && fade.to[index] == 0.0 && volumes[index] == 0.0 {
                sinks[index].clear();
                paths[index] = None;
            }
        }
    }
    drop(sinks);
    drop(thunder);
    drop(stream);
}

/// Pure value used by boundary tests and the audio worker's interpolation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Crossfade {
    from: [f32; 2],
    to: [f32; 2],
}

impl Crossfade {
    fn sample(self, t: f32) -> [f32; 2] {
        std::array::from_fn(|index| {
            self.from[index] + (self.to[index] - self.from[index]) * t.clamp(0.0, 1.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_rejects_nonfinite_and_clamps() {
        assert_eq!(safe_gain(f32::NAN), 0.0);
        assert_eq!(safe_gain(f32::INFINITY), 0.0);
        assert_eq!(safe_gain(-1.0), 0.0);
        assert_eq!(safe_gain(2.0), 1.0);
        assert_eq!(safe_gain(0.4), 0.4);
    }

    #[test]
    fn crossfade_switches_tracks_and_fades_to_silence() {
        let switch = Crossfade {
            from: [1.0, 0.0],
            to: [0.0, 1.0],
        };
        assert_eq!(switch.sample(0.0), [1.0, 0.0]);
        assert_eq!(switch.sample(1.0), [0.0, 1.0]);
        let silence = Crossfade {
            from: [0.0, 1.0],
            to: [0.0, 0.0],
        };
        assert_eq!(silence.sample(1.0), [0.0, 0.0]);
        let return_to_a = Crossfade {
            from: switch.sample(0.5),
            to: [1.0, 0.0],
        };
        assert_eq!(return_to_a.sample(0.0), [0.5, 0.5]);
        assert_eq!(return_to_a.sample(1.0), [1.0, 0.0]);
    }

    #[test]
    fn returning_to_loaded_track_reuses_its_sink() {
        let paths = [Some(PathBuf::from("A.ogg")), Some(PathBuf::from("B.ogg"))];
        assert_eq!(
            existing_slot(&paths, &Some(PathBuf::from("A.ogg"))),
            Some(0)
        );
        assert_eq!(existing_slot(&paths, &Some(PathBuf::from("C.ogg"))), None);
        assert_eq!(existing_slot(&paths, &None), None);
    }

    #[test]
    fn biome_lookup_rejects_empty_malformed_nonfinite_and_outside_samples() {
        use spall_ecology::cover::CoverField;
        let empty = CoverField::default();
        assert!(empty.expand().unwrap().width() == 0);
        assert_eq!(biome_at(&empty, &empty.expand().unwrap(), [0.0; 3]), None);
        let field = CoverField {
            width: 1,
            rle: vec![1, 0, 1, 0, 1, 0, 1, 0],
        };
        let grid = field.expand().unwrap();
        assert_eq!(biome_at(&field, &grid, [0.0, 0.0, 0.0]), Some(0));
        assert_eq!(biome_at(&field, &grid, [4.0, 0.0, 0.0]), None);
        assert_eq!(biome_at(&field, &grid, [f64::NAN, 0.0, 0.0]), None);
        let malformed = CoverField {
            width: 1,
            rle: vec![1, 0],
        };
        assert!(malformed.expand().is_err());
    }
}
