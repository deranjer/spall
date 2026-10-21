//! Surface-aware footstep sounds. The game supplies a table of surfaces (one
//! recording each) and which terrain materials map to which; the client picks a
//! surface under every foot, slices that surface's recording into single steps
//! and plays a randomly chosen one with a little pitch and gain variation.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime};

use rodio::buffer::SamplesBuffer;
use rodio::{Decoder, OutputStreamBuilder, Source};

/// One row of the game's footstep table.
#[derive(Debug, Clone, PartialEq)]
pub struct FootstepSurface {
    pub name: String,
    /// Recording of several steps in a row; `None` falls back to surface 0.
    pub file: Option<PathBuf>,
    /// Loudness of this surface relative to the others.
    pub gain: f32,
    /// Playback-rate multiplier (below 1 sounds heavier and duller).
    pub pitch: f32,
}

/// Game-owned footstep tuning. Surface 0 is the quiet generic default every
/// other surface falls back to when it has no recording.
#[derive(Debug, Clone, Default)]
pub struct FootstepConfig {
    pub surfaces: Vec<FootstepSurface>,
    /// Terrain material id to surface index.
    pub material_surface: HashMap<u16, usize>,
    pub snow_surface: Option<usize>,
    pub water_surface: Option<usize>,
    /// Snow thinner than this (metres) is not heard underfoot.
    pub snow_min_depth_m: f32,
    /// Ground covered by one step at a walk and at a run, metres.
    pub walk_step_m: f32,
    pub run_step_m: f32,
    /// Ground speeds (m/s) above which a player is walking and running.
    pub walk_min_speed_m_s: f32,
    pub run_min_speed_m_s: f32,
}

impl FootstepConfig {
    /// The surface under a foot: snow deep enough to hear, then water, then the
    /// material of the cell below the foot, else the generic default (0).
    pub fn select_surface(
        &self,
        material_below: Option<u16>,
        snow_depth_m: f32,
        in_water: bool,
    ) -> usize {
        if snow_depth_m >= self.snow_min_depth_m
            && let Some(snow) = self.snow_surface
        {
            return snow;
        }
        if in_water && let Some(water) = self.water_surface {
            return water;
        }
        material_below
            .and_then(|material| self.material_surface.get(&material).copied())
            .filter(|&surface| surface < self.surfaces.len())
            .unwrap_or(0)
    }
}

/// One footfall found by [`FootstepTracker::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footfall {
    pub running: bool,
}

/// Counts ground covered into footfalls: one per step length, only while
/// grounded and moving, so cadence follows the walk and run clips' strides.
#[derive(Debug, Clone, Copy, Default)]
pub struct FootstepTracker {
    travelled_m: f32,
    stepping: bool,
}

impl FootstepTracker {
    pub fn advance(
        &mut self,
        config: &FootstepConfig,
        dt_s: f32,
        ground_speed_m_s: f32,
        grounded: bool,
    ) -> Option<Footfall> {
        let moving = ground_speed_m_s > config.walk_min_speed_m_s && dt_s >= 0.0;
        if !grounded || !moving {
            self.stepping = false;
            return None;
        }
        let running = ground_speed_m_s > config.run_min_speed_m_s;
        let step_m = if running {
            config.run_step_m
        } else {
            config.walk_step_m
        };
        if step_m.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        if !self.stepping {
            // Setting off: the first foot comes down half a step in.
            self.stepping = true;
            self.travelled_m = step_m * 0.5;
        }
        self.travelled_m += ground_speed_m_s * dt_s;
        if self.travelled_m >= step_m {
            self.travelled_m %= step_m;
            Some(Footfall { running })
        } else {
            None
        }
    }
}

/// Loudness and pan of another player's step heard from `distance_m` away,
/// with `right_m` the offset along the listener's right-hand axis.
pub fn heard_step(distance_m: f32, right_m: f32) -> Option<(f32, f32)> {
    const AUDIBLE_M: f32 = 32.0;
    if !distance_m.is_finite() || !right_m.is_finite() || distance_m >= AUDIBLE_M {
        return None;
    }
    let fade = 1.0 - distance_m / AUDIBLE_M;
    let pan = if distance_m > 0.5 {
        (right_m / distance_m).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    Some((fade * fade, pan))
}

/// Splits mono `samples` into single steps: a step starts where the envelope
/// rises above a fraction of the loudest hit and ends once it has stayed below
/// that for a quiet gap. Returns sample ranges.
pub fn slice_steps(samples: &[f32], sample_rate: u32) -> Vec<Range<usize>> {
    const BLOCK_S: f32 = 0.005;
    const QUIET_GAP_S: f32 = 0.15;
    const LEAD_S: f32 = 0.01;
    const TAIL_S: f32 = 0.05;
    const MIN_STEP_S: f32 = 0.06;
    const MAX_STEP_S: f32 = 0.7;
    const THRESHOLD: f32 = 0.12;

    let block = ((sample_rate as f32 * BLOCK_S) as usize).max(1);
    let envelope: Vec<f32> = samples
        .chunks(block)
        .map(|chunk| chunk.iter().fold(0.0_f32, |peak, s| peak.max(s.abs())))
        .collect();
    let peak = envelope.iter().copied().fold(0.0_f32, f32::max);
    if peak <= 0.0 {
        return Vec::new();
    }
    let threshold = peak * THRESHOLD;
    let gap_blocks = (QUIET_GAP_S / BLOCK_S) as usize;
    let to_samples = |seconds: f32| (seconds * sample_rate as f32) as usize;

    let mut steps = Vec::new();
    let mut start: Option<usize> = None;
    let mut last_loud = 0;
    for (index, level) in envelope.iter().enumerate() {
        if *level >= threshold {
            if start.is_none() {
                start = Some(index);
            }
            last_loud = index;
        } else if start.is_some() && index - last_loud >= gap_blocks {
            steps.extend(finish_step(
                start.take().unwrap_or(0),
                last_loud,
                block,
                samples.len(),
                (to_samples(LEAD_S), to_samples(TAIL_S)),
                (to_samples(MIN_STEP_S), to_samples(MAX_STEP_S)),
            ));
        }
    }
    if let Some(start) = start {
        steps.extend(finish_step(
            start,
            last_loud,
            block,
            samples.len(),
            (to_samples(LEAD_S), to_samples(TAIL_S)),
            (to_samples(MIN_STEP_S), to_samples(MAX_STEP_S)),
        ));
    }
    steps
}

fn finish_step(
    first_block: usize,
    last_block: usize,
    block: usize,
    len: usize,
    (lead, tail): (usize, usize),
    (min, max): (usize, usize),
) -> Option<Range<usize>> {
    let start = (first_block * block).saturating_sub(lead);
    let end = ((last_block + 1) * block + tail).min(len);
    (end - start >= min).then(|| start..end.min(start + max))
}

/// Individual steps of one recording, mono.
struct StepBank {
    sample_rate: u32,
    steps: Vec<Vec<f32>>,
}

fn load_bank(path: &Path) -> Result<StepBank, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let decoder = Decoder::try_from(file).map_err(|error| error.to_string())?;
    let channels = usize::from(decoder.channels()).max(1);
    let sample_rate = decoder.sample_rate();
    let interleaved: Vec<f32> = decoder.collect();
    let mono: Vec<f32> = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    let steps: Vec<Vec<f32>> = slice_steps(&mono, sample_rate)
        .into_iter()
        .map(|range| mono[range].to_vec())
        .collect();
    if steps.is_empty() {
        return Err("no steps found in recording".into());
    }
    Ok(StepBank { sample_rate, steps })
}

/// How many single steps `path` slices into.
pub fn step_count(path: &Path) -> Result<usize, String> {
    load_bank(path).map(|bank| bank.steps.len())
}

/// A step to play: which surface, how loud (0..=1 before surface gain), where
/// in the stereo field (-1 left .. 1 right) and whether it is a run step.
#[derive(Debug, Clone, Copy)]
pub struct FootstepEvent {
    pub surface: usize,
    pub gain: f32,
    pub pan: f32,
    pub running: bool,
}

/// Plays footsteps on a worker that owns its own output stream.
#[derive(Clone)]
pub struct FootstepAudio {
    tx: SyncSender<FootstepEvent>,
    gain: Arc<AtomicU32>,
}

impl FootstepAudio {
    pub fn start(config: &FootstepConfig, gain: f32) -> Option<Self> {
        if std::env::var_os("SPALL_AUDIO").is_some_and(|value| value == "0")
            || config.surfaces.is_empty()
        {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(16);
        let gain = Arc::new(AtomicU32::new(gain.clamp(0.0, 1.0).to_bits()));
        let worker_gain = Arc::clone(&gain);
        let surfaces = config.surfaces.clone();
        thread::Builder::new()
            .name("spall-footsteps".into())
            .spawn(move || run_footsteps(rx, surfaces, worker_gain))
            .ok()?;
        Some(Self { tx, gain })
    }

    pub fn set_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Drops the step when the worker is behind; returns false once it has stopped.
    pub fn play(&self, event: FootstepEvent) -> bool {
        !matches!(
            self.tx.try_send(event),
            Err(mpsc::TrySendError::Disconnected(_))
        )
    }
}

enum Bank {
    Unloaded,
    Missing,
    Ready(StepBank, usize),
}

/// Small xorshift generator: step variation needs no statistical quality.
struct Variation(u64);

impl Variation {
    fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in `-1..1`.
    fn signed(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

/// Loads a surface's bank on first use. A missing or unreadable recording is
/// reported once (info) and the surface plays the default instead.
fn bank_for<'a>(
    banks: &'a mut [Bank],
    surfaces: &[FootstepSurface],
    index: usize,
) -> Option<&'a mut Bank> {
    if matches!(banks[index], Bank::Unloaded) {
        let surface = &surfaces[index];
        banks[index] = match surface.file.as_deref().map(load_bank) {
            Some(Ok(bank)) => {
                if std::env::var_os("SPALL_FOOTSTEP_LOG").is_some() {
                    eprintln!(
                        "spall-footsteps: {} sliced into {} steps",
                        surface.name,
                        bank.steps.len()
                    );
                }
                Bank::Ready(bank, usize::MAX)
            }
            Some(Err(error)) => {
                tracing::info!(surface = %surface.name, %error, "footstep recording unavailable; using the default");
                Bank::Missing
            }
            None => Bank::Missing,
        };
    }
    match &mut banks[index] {
        Bank::Ready(..) => Some(&mut banks[index]),
        _ => None,
    }
}

fn run_footsteps(
    rx: mpsc::Receiver<FootstepEvent>,
    surfaces: Vec<FootstepSurface>,
    gain: Arc<AtomicU32>,
) {
    let stream = match OutputStreamBuilder::open_default_stream() {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "footstep audio output unavailable; continuing without it");
            return;
        }
    };
    let mut banks: Vec<Bank> = surfaces.iter().map(|_| Bank::Unloaded).collect();
    let mut variation = Variation::new();
    let log = std::env::var_os("SPALL_FOOTSTEP_LOG").is_some();
    while let Ok(event) = rx.recv() {
        let master = f32::from_bits(gain.load(Ordering::Relaxed));
        if master <= 0.0 {
            continue;
        }
        let requested = event.surface.min(surfaces.len() - 1);
        let index = if bank_for(&mut banks, &surfaces, requested).is_some() {
            requested
        } else if bank_for(&mut banks, &surfaces, 0).is_some() {
            0
        } else {
            continue;
        };
        let Bank::Ready(bank, last) = &mut banks[index] else {
            continue;
        };
        // A different step than the last one on this surface.
        let mut pick = (variation.next() % bank.steps.len() as u64) as usize;
        if pick == *last && bank.steps.len() > 1 {
            pick = (pick + 1) % bank.steps.len();
        }
        *last = pick;
        let surface = &surfaces[index];
        let pitch = surface.pitch * (1.0 + 0.06 * variation.signed());
        let loudness = event.gain
            * surface.gain
            * master
            * (1.0 + 0.12 * variation.signed())
            * if event.running { 1.25 } else { 1.0 };
        if log {
            eprintln!(
                "spall-footsteps: {} step {pick} pitch {pitch:.2} loudness {loudness:.2} pan {:.2}",
                surface.name, event.pan
            );
        }
        let mono = &bank.steps[pick];
        // Equal-power pan, with short fades so a slice never clicks.
        let angle = (event.pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
        let (left, right) = (angle.cos() * loudness, angle.sin() * loudness);
        let fade_in = (bank.sample_rate as usize / 500).max(1);
        let fade_out = (bank.sample_rate as usize / 25).max(1);
        let mut data = Vec::with_capacity(mono.len() * 2);
        for (i, sample) in mono.iter().enumerate() {
            let envelope = ((i + 1) as f32 / fade_in as f32)
                .min((mono.len() - i) as f32 / fade_out as f32)
                .min(1.0);
            let value = sample * envelope;
            data.push(value * left);
            data.push(value * right);
        }
        let rate = (bank.sample_rate as f32 * pitch).round().max(1000.0) as u32;
        stream
            .mixer()
            .add(SamplesBuffer::new(2, rate, data).take_duration(Duration::from_secs(2)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FootstepConfig {
        let surface = |name: &str| FootstepSurface {
            name: name.into(),
            file: None,
            gain: 1.0,
            pitch: 1.0,
        };
        FootstepConfig {
            surfaces: vec![surface("default"), surface("snow"), surface("gravel")],
            material_surface: HashMap::from([(7, 2), (9, 99)]),
            snow_surface: Some(1),
            water_surface: None,
            snow_min_depth_m: 0.01,
            walk_step_m: 1.2,
            run_step_m: 2.1,
            walk_min_speed_m_s: 0.3,
            run_min_speed_m_s: 6.0,
        }
    }

    #[test]
    fn surface_follows_snow_depth_then_material_then_default() {
        let config = config();
        assert_eq!(config.select_surface(Some(7), 0.0, false), 2);
        assert_eq!(config.select_surface(Some(7), 0.009, false), 2);
        assert_eq!(config.select_surface(Some(7), 0.02, false), 1);
        assert_eq!(config.select_surface(Some(8), 0.0, false), 0);
        assert_eq!(config.select_surface(None, 0.0, false), 0);
        // A mapping past the table is ignored rather than trusted.
        assert_eq!(config.select_surface(Some(9), 0.0, false), 0);
    }

    #[test]
    fn water_wins_over_the_material_below_when_the_table_has_it() {
        let mut config = config();
        assert_eq!(config.select_surface(Some(7), 0.0, true), 2);
        config.water_surface = Some(2);
        assert_eq!(config.select_surface(Some(8), 0.0, true), 2);
    }

    #[test]
    fn cadence_needs_grounded_motion_and_runs_faster() {
        let config = config();
        let count = |speed: f32, grounded: bool| {
            let mut tracker = FootstepTracker::default();
            (0..600)
                .filter(|_| tracker.advance(&config, 0.01, speed, grounded).is_some())
                .count()
        };
        // Six seconds: 4 m/s walking covers 24 m, 8 m/s running covers 48 m.
        assert_eq!(count(4.0, true), 20);
        assert_eq!(count(8.0, true), 23);
        assert_eq!(count(4.0, false), 0);
        assert_eq!(count(0.1, true), 0);
        let mut tracker = FootstepTracker::default();
        let first = (0..100).find_map(|i| {
            tracker
                .advance(&config, 0.01, 8.0, true)
                .map(|f| (i, f.running))
        });
        assert_eq!(first.map(|(_, running)| running), Some(true));
        // Leaving the ground restarts the stride.
        tracker.advance(&config, 0.01, 8.0, false);
        assert!(tracker.advance(&config, 0.01, 8.0, true).is_none());
    }

    #[test]
    fn distant_steps_fade_and_pan_to_their_side() {
        assert!(heard_step(40.0, 0.0).is_none());
        let (near, _) = heard_step(2.0, 0.0).unwrap();
        let (far, _) = heard_step(20.0, 0.0).unwrap();
        assert!(near > far);
        assert_eq!(heard_step(10.0, 10.0).unwrap().1, 1.0);
        assert_eq!(heard_step(10.0, -5.0).unwrap().1, -0.5);
    }

    #[test]
    fn slicing_finds_each_burst_between_silences() {
        let rate = 1000;
        let mut samples = vec![0.0_f32; 3000];
        for start in [200, 1200, 2200] {
            for sample in &mut samples[start..start + 100] {
                *sample = 0.8;
            }
        }
        let steps = slice_steps(&samples, rate);
        assert_eq!(steps.len(), 3);
        assert!(steps[0].start <= 200 && steps[0].end >= 300);
        assert!(slice_steps(&[0.0; 500], rate).is_empty());
    }
}
