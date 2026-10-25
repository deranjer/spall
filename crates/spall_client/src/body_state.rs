//! How the local player's body state looks on screen. This is presentation,
//! not gameplay: injury, warmth and wetness all come from the server's
//! authoritative record for this client's own player (ENG-223, ENG-264,
//! ENG-265), and nothing here is sent to the server or decides an outcome.

use spall_render::ScreenGrade;

/// Felt cold and wetness, each 0..=1, eased toward the server's latest values
/// so their once-a-second steps do not show.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BodyState {
    pub cold: f32,
    pub wet: f32,
}

/// Seconds to close most of the gap to the server's latest values.
const EASE_TAU_S: f32 = 1.0;

impl BodyState {
    /// The looks for the server's latest `health` record: cold is lost
    /// warmth. Neutral before the first record arrives.
    pub fn from_server(health: Option<spall_protocol::PlayerHealth>) -> Self {
        health.map_or_else(Self::default, |h| Self {
            cold: 1.0 - f32::from(h.warmth_q8) / 255.0,
            wet: f32::from(h.wetness_q8) / 255.0,
        })
    }

    /// Moves `dt_s` seconds toward `target`.
    pub fn ease_toward(&mut self, target: &BodyState, dt_s: f32) {
        let dt_s = if dt_s.is_finite() {
            dt_s.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let blend = 1.0 - (-dt_s / EASE_TAU_S).exp();
        self.cold += (finite_unit(target.cold) - self.cold) * blend;
        self.wet += (finite_unit(target.wet) - self.wet) * blend;
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

/// Health fraction at and below which the injury look is at full strength.
/// Above it the look grows linearly with lost health, so one bear swipe
/// (40 of 100 left) is past the reminder icon's [`REMINDER_AT`].
const INJURY_FULL_AT: f32 = 0.2;

/// `preview` with its injury raised to what this client's latest
/// authoritative `health` shows (the larger of the two). No health yet, or
/// full health after a respawn, adds nothing.
pub fn with_health(
    preview: &BodyPreview,
    health: Option<spall_protocol::PlayerHealth>,
) -> BodyPreview {
    let injury = health.filter(|h| h.max_health > 0).map_or(0.0, |h| {
        let lost = 1.0 - f32::from(h.health) / f32::from(h.max_health);
        (lost / (1.0 - INJURY_FULL_AT)).clamp(0.0, 1.0)
    });
    BodyPreview {
        injury: finite_unit(preview.injury).max(injury),
        ..*preview
    }
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

    /// The server's record with `warmth` and `wetness` as 0..=1.
    fn server(warmth: f32, wetness: f32) -> Option<spall_protocol::PlayerHealth> {
        Some(spall_protocol::PlayerHealth {
            health: 100,
            max_health: 100,
            warmth_q8: (warmth * 255.0).round() as u8,
            wetness_q8: (wetness * 255.0).round() as u8,
        })
    }

    fn run(state: &mut BodyState, target: BodyState, seconds: f32) {
        for _ in 0..(seconds * 10.0) as u32 {
            state.ease_toward(&target, 0.1);
        }
    }

    #[test]
    fn a_neutral_body_leaves_the_screen_untouched() {
        for neutral in [None, server(1.0, 0.0)] {
            let mut state = BodyState::default();
            run(&mut state, BodyState::from_server(neutral), 60.0);
            assert_eq!(state, BodyState::default());
            for t in [0.0, 0.37, 12.5] {
                assert_eq!(
                    screen_grade(&state, &BodyPreview::default(), t),
                    ScreenGrade::IDENTITY
                );
            }
        }
    }

    #[test]
    fn the_server_s_cold_and_wet_show_smoothly() {
        let target = BodyState::from_server(server(0.1, 1.0));
        assert!(
            (target.cold - 0.9).abs() < 0.01 && target.wet == 1.0,
            "{target:?}"
        );
        // One frame after a step: part of the way, not a snap.
        let mut state = BodyState::default();
        state.ease_toward(&target, 1.0 / 60.0);
        assert!(state.cold > 0.0 && state.cold < 0.1, "{state:?}");
        run(&mut state, target, 5.0);
        let grade = screen_grade(&state, &BodyPreview::default(), 0.0);
        assert!(grade.frost > 0.6 && grade.saturation < 0.8, "{grade:?}");
        // Warmed up and dry again: the frost clears.
        run(&mut state, BodyState::from_server(server(1.0, 0.0)), 15.0);
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
    fn lost_health_shows_as_injury_and_full_health_clears_it() {
        let health = |health| {
            Some(spall_protocol::PlayerHealth {
                health,
                max_health: 100,
                warmth_q8: 255,
                wetness_q8: 0,
            })
        };
        let none = BodyPreview::default();
        let body = BodyState::default();
        for unhurt in [None, health(100)] {
            let preview = with_health(&none, unhurt);
            assert_eq!(screen_grade(&body, &preview, 0.3), ScreenGrade::IDENTITY);
            assert!(reminders(&body, &preview).iter().all(|(_, o)| *o == 0.0));
        }
        // One bear swipe: a red, drained screen and the injury icon.
        let hurt = with_health(&none, health(40));
        let grade = screen_grade(&body, &hurt, 0.0);
        assert!(grade.vignette > 0.3 && grade.saturation < 0.6, "{grade:?}");
        assert!(
            grade.vignette_color[0] > grade.vignette_color[1],
            "{grade:?}"
        );
        assert_eq!(reminders(&body, &hurt)[0].0, BodyCue::Injury);
        assert!(reminders(&body, &hurt)[0].1 > 0.0);
        // A scratch is a faint look and no icon; the admin preview still wins
        // when it is stronger.
        let scratch = with_health(&none, health(90));
        assert!(scratch.injury > 0.0 && scratch.injury < 0.2, "{scratch:?}");
        assert_eq!(reminders(&body, &scratch)[0].1, 0.0);
        let preview = BodyPreview {
            injury: 0.9,
            ..none
        };
        assert_eq!(with_health(&preview, health(90)).injury, 0.9);
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
