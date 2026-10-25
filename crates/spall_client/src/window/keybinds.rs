//! The one table of keyboard bindings (ENG-270). `handler.rs` asks it which
//! [`Action`] a key is; the Esc menu edits it; it is saved to a small JSON
//! file in the platform config directory.
//!
//! Escape is not rebindable: it closes overlays and opens the menu. The
//! mouse (look, hammer, wheel) is not rebindable either.

use std::collections::HashMap;
use std::path::PathBuf;

use winit::keyboard::KeyCode;

/// Everything a key can do in the interactive window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Action {
    Forward,
    Back,
    Left,
    Right,
    Jump,
    Descend,
    Sprint,
    Interact,
    Torch,
    Pack,
    Book,
    Flight,
    Recentre,
    ThirdPerson,
    AdminMenu,
    Screenshot,
    CopyReport,
    ToggleTerrain,
    ToggleBodies,
    ToggleCapsule,
    DebugView,
    Skylight,
    Bounce,
    Lighting,
}

impl Action {
    pub(super) const ALL: [Action; 24] = [
        Action::Forward,
        Action::Back,
        Action::Left,
        Action::Right,
        Action::Jump,
        Action::Descend,
        Action::Sprint,
        Action::Interact,
        Action::Torch,
        Action::Pack,
        Action::Book,
        Action::Flight,
        Action::Recentre,
        Action::ThirdPerson,
        Action::AdminMenu,
        Action::Screenshot,
        Action::CopyReport,
        Action::ToggleTerrain,
        Action::ToggleBodies,
        Action::ToggleCapsule,
        Action::DebugView,
        Action::Skylight,
        Action::Bounce,
        Action::Lighting,
    ];

    /// Name in the settings file.
    pub(super) fn id(self) -> &'static str {
        match self {
            Action::Forward => "forward",
            Action::Back => "back",
            Action::Left => "left",
            Action::Right => "right",
            Action::Jump => "jump",
            Action::Descend => "descend",
            Action::Sprint => "sprint",
            Action::Interact => "interact",
            Action::Torch => "torch",
            Action::Pack => "pack",
            Action::Book => "book",
            Action::Flight => "flight",
            Action::Recentre => "recentre",
            Action::ThirdPerson => "third_person",
            Action::AdminMenu => "admin_menu",
            Action::Screenshot => "screenshot",
            Action::CopyReport => "copy_report",
            Action::ToggleTerrain => "toggle_terrain",
            Action::ToggleBodies => "toggle_bodies",
            Action::ToggleCapsule => "toggle_capsule",
            Action::DebugView => "debug_view",
            Action::Skylight => "skylight",
            Action::Bounce => "bounce",
            Action::Lighting => "lighting",
        }
    }

    /// Label in the menu.
    pub(super) fn label(self) -> &'static str {
        match self {
            Action::Forward => "Walk forward",
            Action::Back => "Walk back",
            Action::Left => "Strafe left",
            Action::Right => "Strafe right",
            Action::Jump => "Jump / rise (flying)",
            Action::Descend => "Descend (flying)",
            Action::Sprint => "Sprint / fly faster",
            Action::Interact => "Interact: pick up, doors",
            Action::Torch => "Place a torch",
            Action::Pack => "Pack",
            Action::Book => "Blueprint book",
            Action::Flight => "Toggle flight",
            Action::Recentre => "Recentre the view",
            Action::ThirdPerson => "Third-person view",
            Action::AdminMenu => "Admin menu",
            Action::Screenshot => "Screenshot",
            Action::CopyReport => "Copy debug report",
            Action::ToggleTerrain => "Toggle terrain",
            Action::ToggleBodies => "Toggle detached bodies",
            Action::ToggleCapsule => "Toggle capsule markers",
            Action::DebugView => "Cycle render debug views",
            Action::Skylight => "Toggle visibility-aware skylight",
            Action::Bounce => "Toggle diffuse bounce",
            Action::Lighting => "Cycle lighting quality",
        }
    }

    pub(super) fn default_key(self) -> KeyCode {
        match self {
            Action::Forward => KeyCode::KeyW,
            Action::Back => KeyCode::KeyS,
            Action::Left => KeyCode::KeyA,
            Action::Right => KeyCode::KeyD,
            Action::Jump => KeyCode::Space,
            Action::Descend => KeyCode::ControlLeft,
            Action::Sprint => KeyCode::ShiftLeft,
            Action::Interact => KeyCode::KeyE,
            Action::Torch => KeyCode::KeyT,
            Action::Pack => KeyCode::Tab,
            Action::Book => KeyCode::KeyB,
            Action::Flight => KeyCode::KeyF,
            Action::Recentre => KeyCode::KeyR,
            Action::ThirdPerson => KeyCode::F8,
            Action::AdminMenu => KeyCode::F10,
            Action::Screenshot => KeyCode::F12,
            Action::CopyReport => KeyCode::F9,
            Action::ToggleTerrain => KeyCode::F1,
            Action::ToggleBodies => KeyCode::F2,
            Action::ToggleCapsule => KeyCode::F3,
            Action::DebugView => KeyCode::F4,
            Action::Skylight => KeyCode::F5,
            Action::Bounce => KeyCode::F6,
            Action::Lighting => KeyCode::F7,
        }
    }
}

/// Why a rebind was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BindError {
    /// The key belongs to another action.
    Conflict(Action),
    /// Escape (and keys the table has no name for) cannot be bound.
    Reserved,
}

/// Right-hand Shift/Ctrl/Alt count as their left-hand twin, as they always did.
fn normalise(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::ShiftRight => KeyCode::ShiftLeft,
        KeyCode::ControlRight => KeyCode::ControlLeft,
        KeyCode::AltRight => KeyCode::AltLeft,
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KeyBindings {
    keys: HashMap<Action, KeyCode>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            keys: Action::ALL.iter().map(|a| (*a, a.default_key())).collect(),
        }
    }
}

impl KeyBindings {
    pub(super) fn key(&self, action: Action) -> KeyCode {
        self.keys[&action]
    }

    /// The action `code` triggers, if any.
    pub(super) fn action_for(&self, code: KeyCode) -> Option<Action> {
        let code = normalise(code);
        Action::ALL.into_iter().find(|a| self.keys[a] == code)
    }

    /// Binds `action` to `code`, unless that key is reserved or taken.
    pub(super) fn rebind(&mut self, action: Action, code: KeyCode) -> Result<(), BindError> {
        let code = normalise(code);
        if code == KeyCode::Escape || key_name(code).is_none() {
            return Err(BindError::Reserved);
        }
        if let Some(other) = self.action_for(code)
            && other != action
        {
            return Err(BindError::Conflict(other));
        }
        self.keys.insert(action, code);
        Ok(())
    }

    /// The settings file's text.
    pub(super) fn to_json(&self) -> String {
        let bindings: serde_json::Map<String, serde_json::Value> = Action::ALL
            .iter()
            .filter_map(|a| Some((a.id().to_owned(), key_name(self.keys[a])?.into())))
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({ "bindings": bindings }))
            .unwrap_or_default()
    }

    /// Reads a settings file's text over the defaults. Unknown actions and
    /// keys, and entries that would clash, are skipped and the default kept.
    pub(super) fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct File {
            bindings: HashMap<String, String>,
        }
        let file: File = serde_json::from_str(text)?;
        let wanted: Vec<(Action, KeyCode)> = Action::ALL
            .iter()
            .filter_map(|a| Some((*a, key_from_name(file.bindings.get(a.id())?)?)))
            .collect();
        // Apply the whole file at once if it is consistent (so a swap of two
        // keys works); otherwise one entry at a time, dropping the clashes.
        let mut whole = Self::default();
        whole.keys.extend(wanted.iter().copied());
        let mut seen = std::collections::HashSet::new();
        let consistent = whole
            .keys
            .values()
            .all(|k| *k != KeyCode::Escape && seen.insert(*k));
        if consistent {
            return Ok(whole);
        }
        let mut out = Self::default();
        for (action, key) in wanted {
            let _ = out.rebind(action, key);
        }
        Ok(out)
    }

    /// Loads the saved bindings, or the defaults if there is no usable file.
    pub(super) fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_json(&text).unwrap_or_else(|error| {
                eprintln!("spall-interactive: ignoring {}: {error}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub(super) fn save(&self) {
        let Some(path) = settings_path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, self.to_json()));
        if let Err(error) = result {
            eprintln!("spall-interactive: saving {}: {error}", path.display());
        }
    }
}

/// `%APPDATA%\Spall\client-settings.json`, or `$XDG_CONFIG_HOME/spall/` or
/// `~/.config/spall/` elsewhere.
fn settings_path() -> Option<PathBuf> {
    let env = |name| std::env::var_os(name).filter(|v| !v.is_empty());
    let dir = if cfg!(windows) {
        PathBuf::from(env("APPDATA")?).join("Spall")
    } else if let Some(xdg) = env("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("spall")
    } else {
        PathBuf::from(env("HOME")?).join(".config").join("spall")
    };
    Some(dir.join("client-settings.json"))
}

macro_rules! key_table {
    ($($name:ident),* $(,)?) => {
        const NAMED_KEYS: &[(KeyCode, &str)] = &[$((KeyCode::$name, stringify!($name))),*];
    };
}

key_table!(
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Space,
    Tab,
    Enter,
    Backspace,
    ShiftLeft,
    ControlLeft,
    AltLeft,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Minus,
    Equal,
    Comma,
    Period,
    Slash,
    Semicolon,
    Quote,
    BracketLeft,
    BracketRight,
    Backslash,
    Backquote,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
);

/// The file/menu name of a bindable key (`None` if it is not bindable).
pub(super) fn key_name(code: KeyCode) -> Option<&'static str> {
    NAMED_KEYS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn key_from_name(name: &str) -> Option<KeyCode> {
    NAMED_KEYS.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

/// A key as shown to the player: `KeyW` as `W`, `ShiftLeft` as `Shift`.
pub(super) fn key_label(code: KeyCode) -> String {
    let name = key_name(code).unwrap_or("?");
    match name {
        "ShiftLeft" => "Shift".into(),
        "ControlLeft" => "Ctrl".into(),
        "AltLeft" => "Alt".into(),
        _ => name
            .strip_prefix("Key")
            .or_else(|| name.strip_prefix("Digit"))
            .unwrap_or(name)
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_keys_the_window_always_used() {
        let b = KeyBindings::default();
        let expect = [
            (KeyCode::KeyW, Action::Forward),
            (KeyCode::KeyS, Action::Back),
            (KeyCode::KeyA, Action::Left),
            (KeyCode::KeyD, Action::Right),
            (KeyCode::Space, Action::Jump),
            (KeyCode::ControlLeft, Action::Descend),
            (KeyCode::ControlRight, Action::Descend),
            (KeyCode::ShiftLeft, Action::Sprint),
            (KeyCode::ShiftRight, Action::Sprint),
            (KeyCode::KeyE, Action::Interact),
            (KeyCode::KeyT, Action::Torch),
            (KeyCode::Tab, Action::Pack),
            (KeyCode::KeyB, Action::Book),
            (KeyCode::KeyF, Action::Flight),
            (KeyCode::KeyR, Action::Recentre),
            (KeyCode::F1, Action::ToggleTerrain),
            (KeyCode::F2, Action::ToggleBodies),
            (KeyCode::F3, Action::ToggleCapsule),
            (KeyCode::F4, Action::DebugView),
            (KeyCode::F5, Action::Skylight),
            (KeyCode::F6, Action::Bounce),
            (KeyCode::F7, Action::Lighting),
            (KeyCode::F8, Action::ThirdPerson),
            (KeyCode::F9, Action::CopyReport),
            (KeyCode::F10, Action::AdminMenu),
            (KeyCode::F12, Action::Screenshot),
        ];
        assert_eq!(expect.len(), Action::ALL.len() + 2);
        for (code, action) in expect {
            assert_eq!(b.action_for(code), Some(action), "{code:?}");
        }
        assert_eq!(b.action_for(KeyCode::Escape), None);
    }

    #[test]
    fn every_default_key_has_a_name_and_names_are_unique() {
        let b = KeyBindings::default();
        for a in Action::ALL {
            assert!(key_name(b.key(a)).is_some(), "{a:?}");
        }
        let mut names: Vec<_> = NAMED_KEYS.iter().map(|(_, n)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), NAMED_KEYS.len());
    }

    #[test]
    fn rebinding_round_trips_through_the_settings_text() {
        let mut b = KeyBindings::default();
        b.rebind(Action::Interact, KeyCode::KeyG).unwrap();
        b.rebind(Action::Pack, KeyCode::KeyI).unwrap();
        let back = KeyBindings::from_json(&b.to_json()).unwrap();
        assert_eq!(back, b);
        assert_eq!(back.action_for(KeyCode::KeyG), Some(Action::Interact));
        assert_eq!(back.action_for(KeyCode::KeyE), None);
    }

    #[test]
    fn conflicts_and_reserved_keys_are_rejected() {
        let mut b = KeyBindings::default();
        assert_eq!(
            b.rebind(Action::Torch, KeyCode::KeyW),
            Err(BindError::Conflict(Action::Forward))
        );
        assert_eq!(b.key(Action::Torch), KeyCode::KeyT);
        assert_eq!(
            b.rebind(Action::Torch, KeyCode::Escape),
            Err(BindError::Reserved)
        );
        assert_eq!(
            b.rebind(Action::Sprint, KeyCode::ControlRight),
            Err(BindError::Conflict(Action::Descend))
        );
        // Re-pressing its own key is fine.
        assert_eq!(b.rebind(Action::Torch, KeyCode::KeyT), Ok(()));
    }

    #[test]
    fn a_bad_settings_file_keeps_defaults_where_it_cannot_apply() {
        let text = r#"{"bindings":{"forward":"KeyW","back":"KeyW","torch":"Nope","pack":"KeyI","ghost":"KeyZ"}}"#;
        let b = KeyBindings::from_json(text).unwrap();
        assert_eq!(b.key(Action::Forward), KeyCode::KeyW);
        assert_eq!(b.key(Action::Back), KeyCode::KeyS, "clash keeps default");
        assert_eq!(b.key(Action::Torch), KeyCode::KeyT, "unknown key");
        assert_eq!(b.key(Action::Pack), KeyCode::KeyI);
        assert!(KeyBindings::from_json("not json").is_err());
    }
}
