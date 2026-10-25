//! What the game's events put on the HUD (ENG-251): an objective line along the
//! top and full-screen banners (a death screen, a win screen). The game says
//! what each event means through [`GameHud`]; this module keeps the result and
//! draws it.
use super::*;

const BANNER_INK: yakui::Color = yakui::Color::rgb(245, 238, 222);

/// The HUD state the game's events have built up.
#[derive(Debug, Default)]
pub(super) struct GameHudState {
    pub(super) objective: Option<String>,
    /// The banner and when it goes away (`None`: never).
    banner: Option<(Banner, Option<Instant>)>,
}

impl GameHudState {
    pub(super) fn apply(&mut self, change: HudChange, now: Instant) {
        match change {
            HudChange::None => {}
            HudChange::Objective(text) => self.objective = Some(text),
            HudChange::Banner(banner) => {
                let until = banner
                    .seconds
                    .map(|s| now + Duration::from_secs_f32(s.max(0.0)));
                self.banner = Some((banner, until));
            }
        }
    }

    /// The banner showing at `now`; one whose time is up is dropped.
    pub(super) fn banner(&mut self, now: Instant) -> Option<&Banner> {
        if self
            .banner
            .as_ref()
            .is_some_and(|(_, until)| until.is_some_and(|until| now >= until))
        {
            self.banner = None;
        }
        self.banner.as_ref().map(|(banner, _)| banner)
    }
}

fn at(x: f32, y: f32, children: &mut dyn FnMut()) {
    yakui::align(yakui::Alignment::TOP_LEFT, || {
        yakui::offset(yakui::Vec2::new(x, y), children);
    });
}

fn line(size: f32, text: &str, color: yakui::Color) {
    let mut t = yakui::widgets::Text::new(size, text.to_owned());
    t.style.color = color;
    t.show();
}

/// The objective, top centre.
pub(super) fn draw_objective(text: &str) {
    yakui::align(yakui::Alignment::TOP_CENTER, || {
        yakui::pad(yakui::widgets::Pad::all(12.0), || {
            yakui::colored_box_container(PANEL_BG.with_alpha(0.55), || {
                yakui::pad(yakui::widgets::Pad::balanced(12.0, 6.0), || {
                    line(16.0, text, BANNER_INK);
                });
            });
        });
    });
}

/// A banner over a darkened screen: a large title and a line of detail.
pub(super) fn draw_banner(banner: &Banner, screen: [f32; 2]) {
    yakui::colored_box(
        yakui::Color::BLACK.with_alpha(0.55),
        yakui::Vec2::from_array(screen),
    );
    // Text has no measured width here, so it starts left of centre by a rough
    // share of its length.
    let left = |size: f32, text: &str| {
        (screen[0] - text.chars().count() as f32 * size * 0.5).max(0.0) * 0.5
    };
    let y = screen[1] * 0.38;
    at(left(44.0, &banner.title), y, &mut || {
        line(44.0, &banner.title, BANNER_INK)
    });
    at(left(20.0, &banner.detail), y + 64.0, &mut || {
        line(20.0, &banner.detail, BANNER_INK.with_alpha(0.85))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_banner_goes_away_and_a_held_one_stays() {
        let now = Instant::now();
        let mut hud = GameHudState::default();
        let banner = |seconds| Banner {
            title: "t".into(),
            detail: "d".into(),
            seconds,
        };
        hud.apply(HudChange::Objective("go north".into()), now);
        hud.apply(HudChange::Banner(banner(Some(5.0))), now);
        assert!(hud.banner(now + Duration::from_secs(4)).is_some());
        assert!(hud.banner(now + Duration::from_secs(6)).is_none());
        hud.apply(HudChange::Banner(banner(None)), now);
        assert!(hud.banner(now + Duration::from_secs(3600)).is_some());
        assert_eq!(hud.objective.as_deref(), Some("go north"));
    }
}
