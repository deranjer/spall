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

const UPDATE: Duration = Duration::from_millis(25);

/// Game-owned Ogg paths, indexed by the stable biome code in world generation.
#[derive(Debug, Clone, Default)]
pub struct BiomeAudioConfig {
    pub tracks: Vec<Option<PathBuf>>,
    pub labels: Vec<String>,
    /// Weather beds; game-owned assets. Several layers can play at once, each
    /// for one WeatherKind code and one band of precipitation.
    pub weather_layers: Vec<WeatherLayer>,
    pub thunder: Option<PathBuf>,
    /// How far (metres) each biome track carries, indexed like `tracks`. A track
    /// without an entry carries [`DEFAULT_RANGE_M`].
    pub ranges_m: Vec<f32>,
    /// Which `tracks` are running water (streams, shores). Weather ducks them
    /// far less than other ambience and they win a mix slot over other layers.
    pub water_tracks: Vec<bool>,
    /// Which `tracks` are mostly birdsong. They play quietly unless a bird
    /// is near (see [`birdsong_level`]), so a looping song does not follow
    /// the player across a whole biome.
    pub birdsong_tracks: Vec<bool>,
    /// Creature species that count as birds for [`birdsong_level`].
    pub bird_species: Vec<String>,
    pub footsteps: crate::footsteps::FootstepConfig,
    pub breathing: crate::breathing::BreathingConfig,
}

/// One looping weather bed: plays at `gain` while the weather is `kind` (the
/// stable WeatherKind code) and its precipitation, 0..=1, lies in
/// `precipitation_min..precipitation_max`.
#[derive(Debug, Clone)]
pub struct WeatherLayer {
    pub kind: u8,
    pub precipitation_min: f32,
    pub precipitation_max: f32,
    pub path: PathBuf,
    pub gain: f32,
}

/// Distance at which a biome track has faded to silence unless configured.
pub const DEFAULT_RANGE_M: f32 = 45.0;
/// Most ambience layers mixed at once.
const MAX_LAYERS: usize = 4;
/// Time constant of a layer's volume approaching its target, so a layer fades
/// in and out over a few seconds instead of switching.
const LAYER_RAMP: Duration = Duration::from_millis(1200);

/// Birdsong tracks play at this fraction with no bird near.
const BIRDSONG_FLOOR: f32 = 0.15;
/// A bird this close lifts birdsong to full volume...
const BIRD_NEAR_M: f64 = 15.0;
/// ...fading back to [`BIRDSONG_FLOOR`] by this distance.
const BIRD_FAR_M: f64 = 60.0;

/// Gain for birdsong tracks given the distance to the nearest bird, if any:
/// faint everywhere, full when a bird is close.
pub fn birdsong_level(nearest_bird_m: Option<f64>) -> f32 {
    let near = nearest_bird_m.map_or(0.0, |distance| {
        ((BIRD_FAR_M - distance) / (BIRD_FAR_M - BIRD_NEAR_M)).clamp(0.0, 1.0) as f32
    });
    BIRDSONG_FLOOR + (1.0 - BIRDSONG_FLOOR) * near
}

/// How long a birdsong burst lasts, and the rest between bursts with no bird
/// near and with one near, seconds (min, max).
const SONG_S: (f32, f32) = (3.0, 8.0);
const REST_FAR_S: (f32, f32) = (40.0, 120.0);
const REST_NEAR_S: (f32, f32) = (4.0, 15.0);

/// Birdsong comes and goes: short bursts with long silences, the silences
/// much shorter while a bird is near. The looping birdsong tracks are only
/// audible during a burst (each fades over the layer ramp).
#[derive(Debug, Clone)]
pub struct BirdsongGate {
    singing: bool,
    until: Instant,
    rng: u64,
}

impl BirdsongGate {
    /// Starts resting; `seed` varies the schedule between clients.
    pub fn new(now: Instant, seed: u64) -> Self {
        let mut gate = Self {
            singing: false,
            until: now,
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
        };
        gate.until = now + gate.span(REST_NEAR_S);
        gate
    }

    /// Whether birdsong is audible at `now`, advancing the schedule. A bird
    /// coming near cuts a long rest short.
    pub fn singing(&mut self, now: Instant, bird_near: bool) -> bool {
        if bird_near && !self.singing {
            let cap = now + Duration::from_secs_f32(REST_NEAR_S.1);
            self.until = self.until.min(cap);
        }
        if now >= self.until {
            self.singing = !self.singing;
            let span = if self.singing {
                SONG_S
            } else if bird_near {
                REST_NEAR_S
            } else {
                REST_FAR_S
            };
            self.until = now + self.span(span);
        }
        self.singing
    }

    fn span(&mut self, (min, max): (f32, f32)) -> Duration {
        // splitmix64: deterministic and dependency-free.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let unit = ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32;
        Duration::from_secs_f32(min + (max - min) * unit)
    }
}

/// Whether a bird at this distance counts as near for [`BirdsongGate`].
pub fn bird_near(nearest_bird_m: Option<f64>) -> bool {
    nearest_bird_m.is_some_and(|distance| distance < BIRD_FAR_M)
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

    /// The weather beds to play, with their gains, for a kind and precipitation.
    pub fn weather_mix(&self, kind: u8, precipitation: f32) -> Vec<(PathBuf, f32)> {
        self.weather_layers
            .iter()
            .filter(|layer| {
                layer.kind == kind
                    && precipitation >= layer.precipitation_min
                    && precipitation < layer.precipitation_max
            })
            .map(|layer| (layer.path.clone(), layer.gain))
            .collect()
    }

    pub fn is_birdsong(&self, biome: u8) -> bool {
        self.birdsong_tracks
            .get(usize::from(biome))
            .copied()
            .unwrap_or(false)
    }

    pub fn is_water(&self, biome: u8) -> bool {
        self.water_tracks
            .get(usize::from(biome))
            .copied()
            .unwrap_or(false)
    }

    pub fn range_m(&self, biome: u8) -> f32 {
        self.ranges_m
            .get(usize::from(biome))
            .copied()
            .filter(|range| range.is_finite() && *range > 0.0)
            .unwrap_or(DEFAULT_RANGE_M)
    }
}

/// Side of one climate-field square in metres (`FIELD_CELLS` quarter-metre cells).
const FIELD_SQUARE_M: f64 = 4.0;
/// Furthest any biome is searched for, whatever its configured range.
const MAX_SEARCH_M: f64 = 130.0;

/// The ambience layers to play at `position_m`: every biome track whose biome
/// lies within that track's range, each at a volume that falls off smoothly
/// with the distance to the nearest climate square of that biome. The biome
/// under the listener is always full volume, so a stream carries into the
/// meadow beside it and fades out gradually instead of cutting at its bank.
///
/// `weather_duck` (0..=1, 1 is fair weather) scales every layer, but water
/// tracks only lose a quarter of the ducking, and water layers sort ahead of
/// other layers so a full mix never drops a nearby stream for a quieter bird.
pub fn ambience_mix(
    weather_duck: f32,
    birdsong: f32,
    field: &spall_ecology::cover::CoverField,
    grid: &spall_ecology::cover::CoverGrid,
    position_m: [f64; 3],
    config: &BiomeAudioConfig,
) -> Vec<(PathBuf, f32)> {
    let mut nearest: Vec<(u8, f64)> = Vec::new();
    let mut note = |biome: u8, distance: f64| match nearest.iter_mut().find(|(b, _)| *b == biome) {
        Some(entry) => entry.1 = entry.1.min(distance),
        None => nearest.push((biome, distance)),
    };
    if let Some(biome) = biome_at(field, grid, position_m) {
        note(biome, 0.0);
    }
    // Exact nearest-square search over the window the longest track can reach.
    let reach = config
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.is_some())
        .map(|(biome, _)| f64::from(config.range_m(biome as u8)))
        .fold(0.0_f64, f64::max)
        .min(MAX_SEARCH_M);
    let steps = (reach / FIELD_SQUARE_M).ceil() as i64;
    let centre_x = (position_m[0] / FIELD_SQUARE_M).floor();
    let centre_z = (position_m[2] / FIELD_SQUARE_M).floor();
    if centre_x.is_finite() && centre_z.is_finite() {
        for dz in -steps..=steps {
            for dx in -steps..=steps {
                let square_x = centre_x + dx as f64;
                let square_z = centre_z + dz as f64;
                let middle = [
                    (square_x + 0.5) * FIELD_SQUARE_M,
                    position_m[1],
                    (square_z + 0.5) * FIELD_SQUARE_M,
                ];
                // Distance to the nearest point of the square, not its middle.
                let off_x = ((middle[0] - position_m[0]).abs() - FIELD_SQUARE_M * 0.5).max(0.0);
                let off_z = ((middle[2] - position_m[2]).abs() - FIELD_SQUARE_M * 0.5).max(0.0);
                let distance = off_x.hypot(off_z);
                if distance > reach {
                    continue;
                }
                if let Some(biome) = biome_at(field, grid, middle) {
                    note(biome, distance);
                }
            }
        }
    }
    let mut layers: Vec<(PathBuf, f32, bool)> = Vec::new();
    for (biome, distance) in nearest {
        let Some(path) = config.track(biome) else {
            continue;
        };
        let volume = (1.0 - distance / f64::from(config.range_m(biome)))
            .clamp(0.0, 1.0)
            .powf(1.4) as f32;
        if volume < 0.02 {
            continue;
        }
        let water = config.is_water(biome);
        let duck = if water {
            1.0 - 0.25 * (1.0 - weather_duck)
        } else {
            weather_duck
        };
        let volume = volume * duck.clamp(0.0, 1.0);
        let volume = if config.is_birdsong(biome) {
            volume * birdsong.clamp(0.0, 1.0)
        } else {
            volume
        };
        match layers.iter_mut().find(|(existing, _, _)| existing == path) {
            Some(entry) => {
                entry.1 = entry.1.max(volume);
                entry.2 |= water;
            }
            None => layers.push((path.clone(), volume, water)),
        }
    }
    prioritise(layers)
}

/// Water layers first, then loudest, capped at [`MAX_LAYERS`].
fn prioritise(mut layers: Vec<(PathBuf, f32, bool)>) -> Vec<(PathBuf, f32)> {
    layers.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    layers.truncate(MAX_LAYERS);
    layers
        .into_iter()
        .map(|(path, volume, _)| (path, volume))
        .collect()
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

/// Sends desired tracks to a worker that owns the audio device and both sinks.
#[derive(Clone)]
pub struct AmbienceAudio {
    tx: SyncSender<Vec<(PathBuf, f32)>>,
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

    /// Replaces the set of looping layers and their target volumes (0..=1).
    /// Each layer fades toward its target; a layer left out fades to silence.
    pub fn set_mix(
        &self,
        mix: Vec<(PathBuf, f32)>,
    ) -> Result<(), TrySendError<Vec<(PathBuf, f32)>>> {
        self.tx.try_send(mix)
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

struct Layer {
    path: PathBuf,
    sink: Sink,
    volume: f32,
    target: f32,
}

/// One step of a layer volume approaching its target with time constant
/// [`LAYER_RAMP`].
fn ramp(volume: f32, target: f32, dt: Duration) -> f32 {
    let blend = 1.0 - (-dt.as_secs_f32() / LAYER_RAMP.as_secs_f32()).exp();
    volume + (target - volume) * blend
}

fn run_audio(
    rx: mpsc::Receiver<Vec<(PathBuf, f32)>>,
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
    let thunder = Sink::connect_new(stream.mixer());
    let mut layers: Vec<Layer> = Vec::new();
    let mut last_update = Instant::now();

    loop {
        match rx.recv_timeout(UPDATE) {
            Ok(request) => {
                for layer in &mut layers {
                    layer.target = 0.0;
                }
                for (path, volume) in request.into_iter().take(MAX_LAYERS) {
                    let volume = safe_gain(volume);
                    if let Some(layer) = layers.iter_mut().find(|layer| layer.path == path) {
                        layer.target = volume;
                        continue;
                    }
                    // Make room by dropping the quietest layer that is already leaving.
                    if layers.len() >= MAX_LAYERS {
                        let leaving = layers
                            .iter()
                            .enumerate()
                            .filter(|(_, layer)| layer.target == 0.0)
                            .min_by(|a, b| a.1.volume.total_cmp(&b.1.volume))
                            .map(|(index, _)| index);
                        match leaving {
                            Some(index) => {
                                layers.remove(index);
                            }
                            None => continue,
                        }
                    }
                    match std::fs::File::open(&path)
                        .map_err(|error| error.to_string())
                        .and_then(|file| {
                            Decoder::new_looped(file).map_err(|error| error.to_string())
                        }) {
                        Ok(source) => {
                            let sink = Sink::connect_new(stream.mixer());
                            sink.set_volume(0.0);
                            sink.append(source);
                            sink.play();
                            layers.push(Layer {
                                path,
                                sink,
                                volume: 0.0,
                                target: volume,
                            });
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not load biome ambience; skipping layer");
                        }
                    }
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
        let now = Instant::now();
        let dt = now.saturating_duration_since(last_update);
        last_update = now;
        for layer in &mut layers {
            layer.volume = ramp(layer.volume, layer.target, dt);
            layer.sink.set_volume(layer.volume * gain);
        }
        layers.retain(|layer| layer.target > 0.0 || layer.volume > 0.005);
    }
    drop(layers);
    drop(thunder);
    drop(stream);
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
    fn layer_volume_approaches_its_target_without_overshoot() {
        let step = Duration::from_millis(100);
        let mut volume = 0.0;
        let mut previous = volume;
        for _ in 0..60 {
            volume = ramp(volume, 1.0, step);
            assert!(volume >= previous && volume <= 1.0);
            previous = volume;
        }
        assert!(volume > 0.99, "{volume}");
        for _ in 0..60 {
            volume = ramp(volume, 0.0, step);
        }
        assert!(volume < 0.01, "{volume}");
        // About one time constant covers 63% of the way.
        let one = ramp(0.0, 1.0, LAYER_RAMP);
        assert!((one - 0.632).abs() < 0.01, "{one}");
    }

    fn two_biome_field() -> (
        spall_ecology::cover::CoverField,
        spall_ecology::cover::CoverGrid,
    ) {
        // A 64 x 64 raster of 4 m squares: biome 0 everywhere except a two-square
        // wide north-south strip of biome 1 (a stream) at x = 60..68 m.
        let width = 64usize;
        let rle = |plane: &[u8]| {
            let mut out = Vec::new();
            let mut i = 0;
            while i < plane.len() {
                let value = plane[i];
                let mut run = 1;
                while i + run < plane.len() && plane[i + run] == value && run < 255 {
                    run += 1;
                }
                out.push(run as u8);
                out.push(value);
                i += run;
            }
            out
        };
        let biome: Vec<u8> = (0..width * width)
            .map(|index| u8::from(matches!(index % width, 15 | 16)))
            .collect();
        let zeros = vec![0u8; width * width];
        let mut bytes = rle(&biome);
        for _ in 0..3 {
            bytes.extend(rle(&zeros));
        }
        let field = spall_ecology::cover::CoverField {
            width: width as u32,
            rle: bytes,
        };
        let grid = field.expand().unwrap();
        (field, grid)
    }

    #[test]
    fn a_nearby_stream_is_mixed_in_and_fades_with_distance() {
        let (field, grid) = two_biome_field();
        let config = BiomeAudioConfig {
            tracks: vec![Some("meadow.ogg".into()), Some("stream.ogg".into())],
            ranges_m: vec![45.0, 90.0],
            water_tracks: vec![false, true],
            ..Default::default()
        };
        let volume_of = |x: f64, name: &str| {
            ambience_mix(1.0, 1.0, &field, &grid, [x, 10.0, 100.0], &config)
                .into_iter()
                .find(|(path, _)| path.to_str() == Some(name))
                .map_or(0.0, |(_, v)| v)
        };
        // Standing on the stream bank: full volume. 30 m away: still clearly audible.
        assert!(volume_of(61.0, "stream.ogg") > 0.99);
        let near = volume_of(30.0, "stream.ogg");
        let far = volume_of(10.0, "stream.ogg");
        assert!(near > 0.2, "30 m from a 90 m stream: {near}");
        assert!(far < near, "{far} vs {near}");
        // Out of range the stream drops out entirely, and the meadow stays.
        assert_eq!(volume_of(-100.0 + 0.0, "stream.ogg"), 0.0);
        assert!(volume_of(10.0, "meadow.ogg") > 0.99);
    }

    #[test]
    fn weather_ducks_other_ambience_but_keeps_water_audible_and_in_the_mix() {
        let (field, grid) = two_biome_field();
        let config = BiomeAudioConfig {
            tracks: vec![Some("meadow.ogg".into()), Some("stream.ogg".into())],
            ranges_m: vec![45.0, 90.0],
            water_tracks: vec![false, true],
            ..Default::default()
        };
        let at = |config: &BiomeAudioConfig, duck: f32| {
            ambience_mix(duck, 1.0, &field, &grid, [61.0, 10.0, 100.0], config)
        };
        let get = |mix: &[(PathBuf, f32)], name: &str| {
            mix.iter()
                .find(|(p, _)| p.to_str() == Some(name))
                .map_or(0.0, |(_, v)| *v)
        };
        let ducked = at(&config, 0.35);
        assert!(get(&ducked, "stream.ogg") > 0.8, "{ducked:?}");
        assert!(get(&ducked, "meadow.ogg") < 0.4, "{ducked:?}");
        // With more layers than mix slots, water still beats louder non-water layers.
        let crowded = prioritise(vec![
            ("a.ogg".into(), 1.0, false),
            ("b.ogg".into(), 0.9, false),
            ("c.ogg".into(), 0.8, false),
            ("d.ogg".into(), 0.7, false),
            ("stream.ogg".into(), 0.1, true),
        ]);
        assert_eq!(crowded.len(), MAX_LAYERS);
        assert!(
            crowded
                .iter()
                .any(|(p, _)| p.to_str() == Some("stream.ogg"))
        );
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

    #[test]
    fn birdsong_is_faint_without_a_bird_and_full_beside_one() {
        let floor = birdsong_level(None);
        assert!(floor > 0.0 && floor < 0.3, "{floor}");
        assert_eq!(birdsong_level(Some(200.0)), floor);
        assert!(birdsong_level(Some(40.0)) > floor);
        assert!((birdsong_level(Some(10.0)) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn birdsong_comes_in_bursts_with_long_rests_unless_a_bird_is_near() {
        let start = Instant::now();
        let mut gate = BirdsongGate::new(start, 7);
        let count = |gate: &mut BirdsongGate, near: bool, from: Instant| {
            (0..1200)
                .filter(|i| gate.singing(from + Duration::from_millis(250) * *i, near))
                .count()
        };
        // Five minutes alone: mostly silent.
        let alone = count(&mut gate, false, start);
        assert!(alone > 0 && alone < 1200 / 4, "{alone}");
        // Five minutes beside a bird: far more song, still not constant.
        let later = start + Duration::from_secs(300);
        let near = count(&mut gate, true, later);
        assert!(near > alone * 2 && near < 1200, "{near} vs {alone}");
    }
}
