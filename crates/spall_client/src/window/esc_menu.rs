//! The Esc menu (ENG-270): resume, the keybind list with click-to-rebind,
//! and quit. Screen-space UI like the pack and book.

use super::keybinds::{Action, BindError, KeyBindings, key_label};
use super::*;

/// The menu's state between frames.
#[derive(Debug, Default)]
pub(super) struct EscMenu {
    pub(super) open: bool,
    /// The action waiting for its new key.
    pub(super) capturing: Option<Action>,
    /// The last rebind refusal, shown under the list.
    pub(super) message: Option<String>,
}

/// What the menu shows this frame.
#[derive(Debug, Clone)]
pub(super) struct EscMenuView {
    pub(super) rows: Vec<(Action, String)>,
    pub(super) capturing: Option<Action>,
    pub(super) message: Option<String>,
}

/// Buttons clicked this frame.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct EscMenuActions {
    pub(super) resume: bool,
    pub(super) quit: bool,
    pub(super) reset_defaults: bool,
    pub(super) start_rebind: Option<Action>,
}

impl EscMenu {
    pub(super) fn view(&self, keys: &KeyBindings) -> EscMenuView {
        EscMenuView {
            rows: Action::ALL
                .iter()
                .map(|a| (*a, key_label(keys.key(*a))))
                .collect(),
            capturing: self.capturing,
            message: self.message.clone(),
        }
    }

    /// A key pressed while an action waits for one. Returns whether the
    /// bindings changed (and so need saving).
    pub(super) fn capture(&mut self, keys: &mut KeyBindings, code: KeyCode) -> bool {
        let Some(action) = self.capturing.take() else {
            return false;
        };
        if code == KeyCode::Escape {
            self.message = None;
            return false;
        }
        match keys.rebind(action, code) {
            Ok(()) => {
                self.message = None;
                true
            }
            Err(BindError::Conflict(other)) => {
                self.message = Some(format!(
                    "{} is already {}",
                    key_label(code),
                    other.label().to_lowercase()
                ));
                false
            }
            Err(BindError::Reserved) => {
                self.message = Some("That key cannot be used".to_owned());
                false
            }
        }
    }
}

const MENU_INK: yakui::Color = yakui::Color::rgb(225, 230, 240);

pub(super) fn draw_esc_menu(view: &EscMenuView, actions: &mut EscMenuActions, screen: [f32; 2]) {
    yakui::colored_box(
        yakui::Color::BLACK.with_alpha(0.5),
        yakui::Vec2::from_array(screen),
    );
    let height = (screen[1] - 60.0).max(160.0);
    yakui::align(yakui::Alignment::CENTER, || {
        yakui::constrained(yakui::Constraints::height(0.0, height), || {
            yakui::colored_box_container(PANEL_BG.with_alpha(0.95), || {
                yakui::pad(yakui::widgets::Pad::all(16.0), || {
                    yakui::column(|| {
                        let mut title = yakui::widgets::Text::new(24.0, "Paused");
                        title.style.color = PANEL_ACCENT;
                        title.show();
                        yakui::row(|| {
                            if yakui::button("Resume").clicked {
                                actions.resume = true;
                            }
                            if yakui::button("Reset keys to default").clicked {
                                actions.reset_defaults = true;
                            }
                            if yakui::button("Quit").clicked {
                                actions.quit = true;
                            }
                        });
                        section("Keybinds (click a key, then press the new one)");
                        yakui::constrained(
                            yakui::Constraints::height(0.0, (height - 190.0).max(80.0)),
                            || {
                                yakui::scroll_vertical(|| {
                                    yakui::column(|| {
                                        for (action, key) in &view.rows {
                                            yakui::row(|| {
                                                yakui::constrained(
                                                    yakui::Constraints::tight(yakui::Vec2::new(
                                                        260.0, 24.0,
                                                    )),
                                                    || {
                                                        let mut t = yakui::widgets::Text::new(
                                                            15.0,
                                                            action.label(),
                                                        );
                                                        t.style.color = MENU_INK;
                                                        t.show();
                                                    },
                                                );
                                                let label = if view.capturing == Some(*action) {
                                                    "press a key...".to_owned()
                                                } else {
                                                    key.clone()
                                                };
                                                if yakui::button(label).clicked {
                                                    actions.start_rebind = Some(*action);
                                                }
                                            });
                                        }
                                    });
                                });
                            },
                        );
                        let mut note = yakui::widgets::Text::new(
                            13.0,
                            view.message.clone().unwrap_or_else(|| {
                                "Esc cancels a rebind. Mouse keys are fixed.".into()
                            }),
                        );
                        note.style.color = if view.message.is_some() {
                            yakui::Color::rgb(240, 120, 100)
                        } else {
                            PANEL_MUTED
                        };
                        note.show();
                    });
                });
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_captured_key_rebinds_and_a_clash_is_refused_with_a_message() {
        let mut keys = KeyBindings::default();
        let mut menu = EscMenu {
            capturing: Some(Action::Torch),
            ..EscMenu::default()
        };
        assert!(menu.capture(&mut keys, KeyCode::KeyG));
        assert_eq!(keys.key(Action::Torch), KeyCode::KeyG);
        assert_eq!(menu.capturing, None);

        menu.capturing = Some(Action::Torch);
        assert!(!menu.capture(&mut keys, KeyCode::KeyW));
        assert_eq!(keys.key(Action::Torch), KeyCode::KeyG);
        assert!(menu.message.as_deref().unwrap().contains("already"));

        menu.capturing = Some(Action::Torch);
        assert!(!menu.capture(&mut keys, KeyCode::Escape));
        assert_eq!(keys.key(Action::Torch), KeyCode::KeyG, "Esc cancels");
        assert_eq!(menu.message, None);
    }
}
