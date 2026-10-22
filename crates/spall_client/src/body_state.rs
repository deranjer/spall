//! How the local player's body state looks on screen. This is presentation,
//! not gameplay: cold and wetness are eased from signals the client already
//! has (the replicated weather, shelter, the replicated water surface), and
//! nothing here is sent to the server or decides an outcome. When survival
//! stats become server-authoritative they replace these inputs.

use spall_render::ScreenGrade;

/// What the body is exposed to this frame.
#[derive(Debug, Clone, Copy)]
pub struct BodyConditions {
    pub air_temperature_c: f32,
    /// 0 fully sheltered, 1 in the open.
    pub exposure: f32,
    /// Falling rain at the player, 0..=1 (snow counts for less).
    pub precipitation: f32,
    /// The body is in water.
    pub submerged: bool,
}

/// Felt cold and wetness, each 0..=1, eased over time so stepping under a
/// roof or out of a lake does not snap the screen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BodyState {
    pub cold: f32,
    pub wet: f32,
}

/// Felt temperature at which the screen starts to look cold, and the span
/// below it over which the look reaches full strength.
const COLD_START_C: f32 = 8.0;
const COLD_SPAN_C: f32 = 20.0;
/// Being soaked feels this many degrees colder; wind in the open, this many.
const WET_CHILL_C: f32 = 8.0;
const WIND_CHILL_C: f32 = 3.0;

impl BodyState {
    pub fn advance(&mut self, conditions: &BodyConditions, dt_s: f32) {
        let dt_s = if dt_s.is_finite() {
            dt_s.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let air = if conditions.air_temperature_c.is_finite() {
            conditions.air_temperature_c
        } else {
            15.0
        };
        let exposure = finite_unit(conditions.exposure);
        let rain = finite_unit(conditions.precipitation) * exposure;
        let (wet_target, wet_tau_s) = if conditions.submerged {
            (1.0, 1.0)
        } else if rain > self.wet {
            (rain, 25.0)
        } else {
            // Drying: about two minutes at 10 C, quicker when warm.
            (rain, 120.0 / (1.0 + air.max(0.0) / 10.0))
        };
        self.wet = ease(self.wet, wet_target, dt_s, wet_tau_s);

        let felt = air - self.wet * WET_CHILL_C - exposure * WIND_CHILL_C;
        let cold_target = ((COLD_START_C - felt) / COLD_SPAN_C).clamp(0.0, 1.0);
        // Chill sets in slower than it lifts beside a fire.
        let cold_tau_s = if cold_target > self.cold { 20.0 } else { 10.0 };
        self.cold = ease(self.cold, cold_target, dt_s, cold_tau_s);
    }
}

/// Looks for states the game does not model yet, set from the admin menu so
/// they can be judged before the stats exist. Each is 0..=1.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BodyPreview {
    pub injury: f32,
    pub thirst: f32,
    pub exhaustion: f32,
    pub hunger: f32,
}

/// The screen grade for `state` plus any `preview` looks at `time_s` (drives
/// the heartbeat pulse). A neutral body gives [`ScreenGrade::IDENTITY`].
pub fn screen_grade(state: &BodyState, preview: &BodyPreview, time_s: f32) -> ScreenGrade {
    let cold = finite_unit(state.cold);
    let wet = finite_unit(state.wet);
    let injury = finite_unit(preview.injury);
    let thirst = finite_unit(preview.thirst);
    let exhaustion = finite_unit(preview.exhaustion);
    let hunger = finite_unit(preview.hunger);

    // Colour drains with injury above all; cold, thirst and hunger dull it.
    let saturation = 1.0 - (0.75 * injury + 0.26 * cold + 0.15 * thirst + 0.25 * hunger).min(0.9);
    // Cold pulls toward blue, being wet slightly darker and cooler.
    let tint = [
        1.0 - 0.075 * cold - 0.06 * wet,
        1.0 - 0.03 * cold - 0.03 * wet,
        1.0 - 0.02 * wet,
    ];
    // Vignettes combine: strengths as independent coverage, colour weighted
    // by strength. The injury vignette beats with the heart.
    // A slow, resting-to-strained pulse: faster flicker read as strobing.
    let beat = heartbeat(time_s, 48.0 + 30.0 * injury);
    let vignettes = [
        (0.35 * injury + 0.25 * injury * beat, [0.22, 0.0, 0.0]),
        (0.55 * exhaustion, [0.0; 3]),
        (0.15 * wet, [0.0; 3]),
    ];
    let mut clear = 1.0;
    let mut weighted = [0.0; 3];
    let mut weight = 0.0;
    for (strength, colour) in vignettes {
        clear *= 1.0 - strength;
        weight += strength;
        for (sum, c) in weighted.iter_mut().zip(colour) {
            *sum += strength * c;
        }
    }
    let vignette_color = if weight > 0.0 {
        weighted.map(|c| c / weight)
    } else {
        [0.0; 3]
    };
    ScreenGrade {
        saturation,
        tint,
        vignette: 1.0 - clear,
        vignette_color,
        edge_blur: (0.8 * thirst + 0.3 * injury + 0.2 * exhaustion).min(1.0),
        // Kept short of full: frost at full strength hid too much of the view.
        frost: 0.75 * smoothstep(0.35, 1.0, cold),
    }
    .clamped()
}

/// A body state with a HUD reminder icon, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyCue {
    Injury,
    Thirst,
    Hunger,
    Exhaustion,
    Cold,
    Wet,
}

impl BodyCue {
    pub const ALL: [Self; 6] = [
        Self::Injury,
        Self::Thirst,
        Self::Hunger,
        Self::Exhaustion,
        Self::Cold,
        Self::Wet,
    ];
}

/// Level at which a state's reminder icon starts to show; it is fully shown
/// [`REMINDER_FADE`] above that.
pub const REMINDER_AT: f32 = 0.7;
const REMINDER_FADE: f32 = 0.1;

/// Each cue's reminder icon opacity, 0..=1, in [`BodyCue::ALL`] order. The
/// screen grade is the main signal; the icon is a backup once a state is bad.
pub fn reminders(state: &BodyState, preview: &BodyPreview) -> [(BodyCue, f32); 6] {
    BodyCue::ALL.map(|cue| {
        let level = finite_unit(match cue {
            BodyCue::Injury => preview.injury,
            BodyCue::Thirst => preview.thirst,
            BodyCue::Hunger => preview.hunger,
            BodyCue::Exhaustion => preview.exhaustion,
            BodyCue::Cold => state.cold,
            BodyCue::Wet => state.wet,
        });
        let opacity = ((level - REMINDER_AT) / REMINDER_FADE).clamp(0.0, 1.0);
        (
            cue,
            if level >= REMINDER_AT {
                opacity.max(0.05)
            } else {
                0.0
            },
        )
    })
}

/// A soft heartbeat at `bpm`: one swell per beat with a faint echo, 0..=1.
fn heartbeat(time_s: f32, bpm: f32) -> f32 {
    let phase = (time_s * bpm / 60.0).rem_euclid(1.0);
    let thump = |at: f32| (-((phase - at) / 0.12).powi(2)).exp();
    thump(0.0).max(thump(1.0)).max(0.3 * thump(0.3))
}

fn ease(value: f32, target: f32, dt_s: f32, tau_s: f32) -> f32 {
    value + (target - value) * (1.0 - (-dt_s / tau_s).exp())
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MILD: BodyConditions = BodyConditions {
        air_temperature_c: 18.0,
        exposure: 1.0,
        precipitation: 0.0,
        submerged: false,
    };

    fn run(state: &mut BodyState, conditions: BodyConditions, seconds: u32) {
        for _ in 0..seconds * 10 {
            state.advance(&conditions, 0.1);
        }
    }

    #[test]
    fn a_neutral_body_leaves_the_screen_untouched() {
        let mut state = BodyState::default();
        run(&mut state, MILD, 60);
        assert_eq!(state, BodyState::default());
        for t in [0.0, 0.37, 12.5] {
            assert_eq!(
                screen_grade(&state, &BodyPreview::default(), t),
                ScreenGrade::IDENTITY
            );
        }
    }

    #[test]
    fn swimming_soaks_quickly_and_drying_takes_minutes() {
        let mut state = BodyState::default();
        run(
            &mut state,
            BodyConditions {
                submerged: true,
                ..MILD
            },
            5,
        );
        assert!(state.wet > 0.95, "{state:?}");
        run(&mut state, MILD, 30);
        assert!(state.wet > 0.4, "dried too fast: {state:?}");
        run(&mut state, MILD, 600);
        assert!(state.wet < 0.01, "{state:?}");
    }

    #[test]
    fn shelter_keeps_rain_off_and_wet_cold_is_colder() {
        let rain = BodyConditions {
            air_temperature_c: 4.0,
            precipitation: 1.0,
            ..MILD
        };
        let mut sheltered = BodyState::default();
        run(
            &mut sheltered,
            BodyConditions {
                exposure: 0.0,
                ..rain
            },
            300,
        );
        let mut open = BodyState::default();
        run(&mut open, rain, 300);
        assert!(
            sheltered.wet < 0.01 && open.wet > 0.9,
            "{sheltered:?} {open:?}"
        );
        assert!(open.cold > sheltered.cold + 0.3, "{sheltered:?} {open:?}");
    }

    #[test]
    fn deep_cold_frosts_the_edges_and_warmth_clears_it() {
        let mut state = BodyState::default();
        run(
            &mut state,
            BodyConditions {
                air_temperature_c: -15.0,
                ..MILD
            },
            180,
        );
        let grade = screen_grade(&state, &BodyPreview::default(), 0.0);
        assert!(grade.frost > 0.7 && grade.saturation < 0.8, "{grade:?}");
        run(&mut state, MILD, 120);
        assert_eq!(
            screen_grade(&state, &BodyPreview::default(), 0.0).frost,
            0.0
        );
    }

    #[test]
    fn every_look_at_full_strength_stays_a_valid_grade() {
        let state = BodyState {
            cold: 1.0,
            wet: 1.0,
        };
        let all = BodyPreview {
            injury: 1.0,
            thirst: 1.0,
            exhaustion: 1.0,
            hunger: 1.0,
        };
        for step in 0..40 {
            let grade = screen_grade(&state, &all, step as f32 * 0.05);
            assert_eq!(grade, grade.clamped());
            assert!(grade.saturation >= 0.1, "never fully grey: {grade:?}");
            assert!(grade.vignette < 1.0, "never a black screen: {grade:?}");
        }
    }

    #[test]
    fn reminder_icons_appear_only_from_seventy_percent() {
        let shown = |state: BodyState, preview: BodyPreview| -> Vec<BodyCue> {
            reminders(&state, &preview)
                .into_iter()
                .filter(|(_, opacity)| *opacity > 0.0)
                .map(|(cue, _)| cue)
                .collect()
        };
        assert!(shown(BodyState::default(), BodyPreview::default()).is_empty());
        let below = BodyPreview {
            injury: 0.69,
            thirst: 0.5,
            exhaustion: 0.0,
            hunger: 0.25,
        };
        assert!(
            shown(
                BodyState {
                    cold: 0.69,
                    wet: 0.6
                },
                below
            )
            .is_empty()
        );
        let over = BodyPreview {
            hunger: 0.75,
            ..BodyPreview::default()
        };
        assert_eq!(
            shown(
                BodyState {
                    cold: 0.7,
                    wet: 1.0
                },
                over
            ),
            [BodyCue::Hunger, BodyCue::Cold, BodyCue::Wet]
        );
        let full = reminders(
            &BodyState {
                cold: 0.85,
                wet: 0.72,
            },
            &over,
        );
        assert_eq!(full[4], (BodyCue::Cold, 1.0));
        assert!(full[5].1 > 0.0 && full[5].1 < 1.0, "fades in: {full:?}");
    }

    #[test]
    fn the_injury_vignette_beats_faster_when_badly_hurt() {
        let beats = |injury: f32| {
            let preview = BodyPreview {
                injury,
                ..BodyPreview::default()
            };
            let samples: Vec<f32> = (0..1000)
                .map(|i| screen_grade(&BodyState::default(), &preview, i as f32 * 0.01).vignette)
                .collect();
            let peak = samples.iter().cloned().fold(0.0, f32::max);
            samples
                .windows(3)
                .filter(|w| w[1] > w[0] && w[1] >= w[2] && w[1] > peak * 0.9)
                .count()
        };
        // 10 s at ~54 vs ~78 bpm.
        assert!(beats(0.2) < beats(1.0), "{} vs {}", beats(0.2), beats(1.0));
    }
}
