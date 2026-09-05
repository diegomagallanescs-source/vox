//! Hotkey chords: parsing, display, and press/release matching.
//!
//! A [`Chord`] is one main key plus a set of modifiers, e.g. `Ctrl+Shift+F13` or `Mouse4`.
//! Keyboard keys are identified by Windows virtual-key code so the platform layer needs no
//! translation table; the names here exist for config files and the settings UI.
//!
//! [`ChordMatcher`] consumes raw press/release events from the low-level hooks and emits
//! [`HotkeyEvent::Pressed`] / [`HotkeyEvent::Released`], telling the caller whether the raw
//! event should be suppressed from the rest of the system.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

/// Windows virtual-key codes used by name in this module.
pub mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12;
    pub const PAUSE: u16 = 0x13;
    pub const CAPITAL: u16 = 0x14;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PRIOR: u16 = 0x21;
    pub const NEXT: u16 = 0x22;
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const INSERT: u16 = 0x2D;
    pub const DELETE: u16 = 0x2E;
    pub const LWIN: u16 = 0x5B;
    pub const RWIN: u16 = 0x5C;
    pub const NUMPAD0: u16 = 0x60;
    /// F1..F24 occupy 0x70..=0x87.
    pub const F1: u16 = 0x70;
    pub const F24: u16 = 0x87;
    pub const SCROLL: u16 = 0x91;
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const LMENU: u16 = 0xA4;
    pub const RMENU: u16 = 0xA5;
    pub const VOLUME_MUTE: u16 = 0xAD;
    pub const VOLUME_DOWN: u16 = 0xAE;
    pub const VOLUME_UP: u16 = 0xAF;
    pub const MEDIA_NEXT_TRACK: u16 = 0xB0;
    pub const MEDIA_PREV_TRACK: u16 = 0xB1;
    pub const MEDIA_STOP: u16 = 0xB2;
    pub const MEDIA_PLAY_PAUSE: u16 = 0xB3;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// "Mouse4" — the back button (XBUTTON1).
    X1,
    /// "Mouse5" — the forward button (XBUTTON2).
    X2,
}

/// A physical input key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// Windows virtual-key code (`VK_*`).
    Keyboard(u16),
    Mouse(MouseButton),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        win: false,
    };

    pub fn is_empty(&self) -> bool {
        !(self.ctrl || self.shift || self.alt || self.win)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl Chord {
    pub fn new(key: Key) -> Self {
        Chord {
            key,
            modifiers: Modifiers::NONE,
        }
    }

    pub fn with_modifiers(key: Key, modifiers: Modifiers) -> Self {
        Chord { key, modifiers }
    }
}

// ---------------------------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------------------------

/// Keys with a fixed display name. Algorithmic names (F-keys, letters, digits, numpad) are
/// handled in code. First match wins for VK -> name, so canonical names come first.
const NAMED_KEYS: &[(&str, u16)] = &[
    ("Backspace", vk::BACK),
    ("Tab", vk::TAB),
    ("Enter", vk::RETURN),
    ("Pause", vk::PAUSE),
    ("CapsLock", vk::CAPITAL),
    ("Escape", vk::ESCAPE),
    ("Space", vk::SPACE),
    ("PageUp", vk::PRIOR),
    ("PageDown", vk::NEXT),
    ("End", vk::END),
    ("Home", vk::HOME),
    ("Left", vk::LEFT),
    ("Up", vk::UP),
    ("Right", vk::RIGHT),
    ("Down", vk::DOWN),
    ("Insert", vk::INSERT),
    ("Delete", vk::DELETE),
    ("ScrollLock", vk::SCROLL),
    ("VolumeMute", vk::VOLUME_MUTE),
    ("VolumeDown", vk::VOLUME_DOWN),
    ("VolumeUp", vk::VOLUME_UP),
    ("MediaNext", vk::MEDIA_NEXT_TRACK),
    ("MediaPrev", vk::MEDIA_PREV_TRACK),
    ("MediaStop", vk::MEDIA_STOP),
    ("MediaPlayPause", vk::MEDIA_PLAY_PAUSE),
    ("LCtrl", vk::LCONTROL),
    ("RCtrl", vk::RCONTROL),
    ("LShift", vk::LSHIFT),
    ("RShift", vk::RSHIFT),
    ("LAlt", vk::LMENU),
    ("RAlt", vk::RMENU),
    ("LWin", vk::LWIN),
    ("RWin", vk::RWIN),
    ("Ctrl", vk::CONTROL),
    ("Shift", vk::SHIFT),
    ("Alt", vk::MENU),
    ("Grave", 0xC0),
    ("Semicolon", 0xBA),
    ("Equals", 0xBB),
    ("Comma", 0xBC),
    ("Minus", 0xBD),
    ("Period", 0xBE),
    ("Slash", 0xBF),
    ("LBracket", 0xDB),
    ("Backslash", 0xDC),
    ("RBracket", 0xDD),
    ("Quote", 0xDE),
];

impl Key {
    /// Human/config-facing name, e.g. `F13`, `A`, `Numpad7`, `Mouse4`, `VK_0xE3`.
    pub fn name(self) -> String {
        match self {
            Key::Mouse(b) => match b {
                MouseButton::Left => "MouseLeft",
                MouseButton::Right => "MouseRight",
                MouseButton::Middle => "MouseMiddle",
                MouseButton::X1 => "Mouse4",
                MouseButton::X2 => "Mouse5",
            }
            .to_string(),
            Key::Keyboard(code) => {
                if (vk::F1..=vk::F24).contains(&code) {
                    return format!("F{}", code - vk::F1 + 1);
                }
                if (0x41..=0x5A).contains(&code) || (0x30..=0x39).contains(&code) {
                    return (code as u8 as char).to_string();
                }
                if (vk::NUMPAD0..=vk::NUMPAD0 + 9).contains(&code) {
                    return format!("Numpad{}", code - vk::NUMPAD0);
                }
                if let Some((name, _)) = NAMED_KEYS.iter().find(|(_, c)| *c == code) {
                    return (*name).to_string();
                }
                format!("VK_0x{:02X}", code)
            }
        }
    }

    /// Inverse of [`Key::name`]; case-insensitive, accepts a few aliases.
    pub fn from_name(s: &str) -> Option<Key> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        let lower = t.to_ascii_lowercase();

        match lower.as_str() {
            "mouse4" | "mouseback" | "xbutton1" => return Some(Key::Mouse(MouseButton::X1)),
            "mouse5" | "mouseforward" | "xbutton2" => return Some(Key::Mouse(MouseButton::X2)),
            "mouse3" | "mousemiddle" => return Some(Key::Mouse(MouseButton::Middle)),
            "mouse1" | "mouseleft" => return Some(Key::Mouse(MouseButton::Left)),
            "mouse2" | "mouseright" => return Some(Key::Mouse(MouseButton::Right)),
            _ => {}
        }

        if let Some(hex) = lower.strip_prefix("vk_0x") {
            return u16::from_str_radix(hex, 16).ok().map(Key::Keyboard);
        }
        if let Some(n) = lower.strip_prefix('f') {
            if let Ok(n) = n.parse::<u16>() {
                if (1..=24).contains(&n) {
                    return Some(Key::Keyboard(vk::F1 + n - 1));
                }
            }
        }
        if let Some(n) = lower.strip_prefix("numpad") {
            if let Ok(n) = n.parse::<u16>() {
                if n <= 9 {
                    return Some(Key::Keyboard(vk::NUMPAD0 + n));
                }
            }
        }
        if t.len() == 1 {
            let c = t.chars().next().unwrap().to_ascii_uppercase();
            if c.is_ascii_alphanumeric() {
                return Some(Key::Keyboard(c as u16));
            }
        }
        NAMED_KEYS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(t))
            .map(|(_, code)| Key::Keyboard(*code))
    }

    /// Which modifier this key represents, if any (left/right/generic variants all map).
    pub fn as_modifier(self) -> Option<Modifier> {
        match self {
            Key::Keyboard(vk::SHIFT | vk::LSHIFT | vk::RSHIFT) => Some(Modifier::Shift),
            Key::Keyboard(vk::CONTROL | vk::LCONTROL | vk::RCONTROL) => Some(Modifier::Ctrl),
            Key::Keyboard(vk::MENU | vk::LMENU | vk::RMENU) => Some(Modifier::Alt),
            Key::Keyboard(vk::LWIN | vk::RWIN) => Some(Modifier::Win),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Win,
}

impl Modifier {
    fn parse(s: &str) -> Option<Modifier> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Modifier::Ctrl),
            "shift" => Some(Modifier::Shift),
            "alt" => Some(Modifier::Alt),
            "win" | "windows" | "super" | "meta" => Some(Modifier::Win),
            _ => None,
        }
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.modifiers;
        if m.ctrl {
            f.write_str("Ctrl+")?;
        }
        if m.shift {
            f.write_str("Shift+")?;
        }
        if m.alt {
            f.write_str("Alt+")?;
        }
        if m.win {
            f.write_str("Win+")?;
        }
        f.write_str(&self.key.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid chord `{0}`")]
pub struct ChordParseError(pub String);

impl FromStr for Chord {
    type Err = ChordParseError;

    /// Parses `Mod+Mod+Key`. All tokens but the last are modifiers.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ChordParseError(s.to_string());
        let tokens: Vec<&str> = s.split('+').map(str::trim).collect();
        let (key_tok, mod_toks) = tokens.split_last().ok_or_else(err)?;
        let key = Key::from_name(key_tok).ok_or_else(err)?;
        let mut modifiers = Modifiers::NONE;
        for tok in mod_toks {
            match Modifier::parse(tok).ok_or_else(err)? {
                Modifier::Ctrl => modifiers.ctrl = true,
                Modifier::Shift => modifiers.shift = true,
                Modifier::Alt => modifiers.alt = true,
                Modifier::Win => modifiers.win = true,
            }
        }
        Ok(Chord { key, modifiers })
    }
}

impl Serialize for Chord {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Chord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(de::Error::custom)
    }
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

/// A raw press/release as delivered by the platform hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub key: Key,
    pub pressed: bool,
}

impl InputEvent {
    pub fn press(key: Key) -> Self {
        InputEvent { key, pressed: true }
    }
    pub fn release(key: Key) -> Self {
        InputEvent {
            key,
            pressed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

/// Result of feeding one raw event to the matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchResult {
    pub event: Option<HotkeyEvent>,
    /// Whether the platform layer should swallow the raw event so other apps never see it.
    pub suppress: bool,
}

impl MatchResult {
    const PASS: MatchResult = MatchResult {
        event: None,
        suppress: false,
    };
}

/// Tracks held keys and detects the bound chord.
///
/// Semantics:
/// * Modifier matching is strict: `F13` does not fire while Ctrl is held.
/// * Modifiers are checked on the main key's *press*; releasing a modifier while the main key
///   is still held does not end the chord (comfortable for hold-to-talk).
/// * Key-repeat of the main key while active is swallowed and produces no event.
/// * The main key's press and release are suppressed while they belong to the chord; all other
///   input passes through untouched.
#[derive(Debug, Clone)]
pub struct ChordMatcher {
    chord: Chord,
    held: HashSet<Key>,
    active: bool,
}

impl ChordMatcher {
    pub fn new(chord: Chord) -> Self {
        ChordMatcher {
            chord,
            held: HashSet::new(),
            active: false,
        }
    }

    pub fn chord(&self) -> Chord {
        self.chord
    }

    /// Replaces the bound chord and clears all state.
    pub fn set_chord(&mut self, chord: Chord) {
        self.chord = chord;
        self.reset();
    }

    /// Forgets held keys. Call on focus loss, session lock, or hook re-install, since the
    /// low-level hook may have missed releases (e.g. across a UAC prompt).
    pub fn reset(&mut self) {
        self.held.clear();
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Modifiers currently held, ignoring `except` (so a chord whose main key *is* a modifier
    /// does not count itself).
    fn held_modifiers(&self, except: Key) -> Modifiers {
        let mut m = Modifiers::NONE;
        for k in &self.held {
            if *k == except {
                continue;
            }
            match k.as_modifier() {
                Some(Modifier::Ctrl) => m.ctrl = true,
                Some(Modifier::Shift) => m.shift = true,
                Some(Modifier::Alt) => m.alt = true,
                Some(Modifier::Win) => m.win = true,
                None => {}
            }
        }
        m
    }

    pub fn feed(&mut self, ev: InputEvent) -> MatchResult {
        if ev.pressed {
            let is_repeat = !self.held.insert(ev.key);
            if ev.key != self.chord.key {
                return MatchResult::PASS;
            }
            if self.active {
                // Auto-repeat of the bound key while held: swallow silently.
                return MatchResult {
                    event: None,
                    suppress: true,
                };
            }
            if !is_repeat && self.held_modifiers(ev.key) == self.chord.modifiers {
                self.active = true;
                return MatchResult {
                    event: Some(HotkeyEvent::Pressed),
                    suppress: true,
                };
            }
            MatchResult::PASS
        } else {
            self.held.remove(&ev.key);
            if ev.key == self.chord.key && self.active {
                self.active = false;
                return MatchResult {
                    event: Some(HotkeyEvent::Released),
                    suppress: true,
                };
            }
            MatchResult::PASS
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(code: u16) -> Key {
        Key::Keyboard(code)
    }
    const F13: Key = Key::Keyboard(vk::F1 + 12);
    const MOUSE4: Key = Key::Mouse(MouseButton::X1);

    #[test]
    fn names_round_trip() {
        let keys = [
            F13,
            kb(vk::F1),
            kb(vk::F24),
            kb(b'A' as u16),
            kb(b'7' as u16),
            kb(vk::NUMPAD0 + 3),
            kb(vk::SPACE),
            kb(vk::MEDIA_PLAY_PAUSE),
            kb(vk::RCONTROL),
            kb(0xE3), // unnamed OEM key
            MOUSE4,
            Key::Mouse(MouseButton::X2),
            Key::Mouse(MouseButton::Middle),
        ];
        for k in keys {
            let name = k.name();
            assert_eq!(Key::from_name(&name), Some(k), "round trip of {name}");
        }
    }

    #[test]
    fn parses_aliases_case_insensitively() {
        assert_eq!(Key::from_name("f13"), Some(F13));
        assert_eq!(Key::from_name("MOUSE4"), Some(MOUSE4));
        assert_eq!(Key::from_name("MouseBack"), Some(MOUSE4));
        assert_eq!(
            Key::from_name("xbutton2"),
            Some(Key::Mouse(MouseButton::X2))
        );
        assert_eq!(Key::from_name("a"), Some(kb(b'A' as u16)));
        assert_eq!(Key::from_name("capslock"), Some(kb(vk::CAPITAL)));
        assert_eq!(Key::from_name("vk_0x1b"), Some(kb(vk::ESCAPE)));
        assert_eq!(Key::from_name("F25"), None);
        assert_eq!(Key::from_name("Numpad10"), None);
        assert_eq!(Key::from_name(""), None);
        assert_eq!(Key::from_name("nonsense"), None);
    }

    #[test]
    fn chord_display_and_parse() {
        let c: Chord = "ctrl + shift + F13".parse().unwrap();
        assert_eq!(c.key, F13);
        assert_eq!(
            c.modifiers,
            Modifiers {
                ctrl: true,
                shift: true,
                alt: false,
                win: false
            }
        );
        assert_eq!(c.to_string(), "Ctrl+Shift+F13");
        assert_eq!(c.to_string().parse::<Chord>().unwrap(), c);

        let m: Chord = "Mouse4".parse().unwrap();
        assert_eq!(m, Chord::new(MOUSE4));
        assert_eq!(m.to_string(), "Mouse4");

        let alone: Chord = "RCtrl".parse().unwrap();
        assert_eq!(alone.key, kb(vk::RCONTROL));
        assert!(alone.modifiers.is_empty());

        assert!("Ctrl+".parse::<Chord>().is_err());
        assert!("Hyper+F1".parse::<Chord>().is_err());
        assert!("".parse::<Chord>().is_err());
    }

    #[test]
    fn chord_serde_as_string() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            chord: Chord,
        }
        let w: Wrap = toml::from_str(r#"chord = "Alt+Mouse5""#).unwrap();
        assert_eq!(w.chord.key, Key::Mouse(MouseButton::X2));
        assert!(w.chord.modifiers.alt);
        assert_eq!(
            toml::to_string(&w).unwrap().trim(),
            r#"chord = "Alt+Mouse5""#
        );
    }

    #[test]
    fn plain_key_press_and_release() {
        let mut m = ChordMatcher::new(Chord::new(F13));
        let r = m.feed(InputEvent::press(F13));
        assert_eq!(
            r,
            MatchResult {
                event: Some(HotkeyEvent::Pressed),
                suppress: true
            }
        );
        assert!(m.is_active());
        let r = m.feed(InputEvent::release(F13));
        assert_eq!(
            r,
            MatchResult {
                event: Some(HotkeyEvent::Released),
                suppress: true
            }
        );
        assert!(!m.is_active());
    }

    #[test]
    fn key_repeat_is_swallowed_without_events() {
        let mut m = ChordMatcher::new(Chord::new(F13));
        m.feed(InputEvent::press(F13));
        for _ in 0..5 {
            let r = m.feed(InputEvent::press(F13));
            assert_eq!(
                r,
                MatchResult {
                    event: None,
                    suppress: true
                }
            );
        }
        assert_eq!(
            m.feed(InputEvent::release(F13)).event,
            Some(HotkeyEvent::Released)
        );
    }

    #[test]
    fn unrelated_keys_pass_through() {
        let mut m = ChordMatcher::new(Chord::new(F13));
        assert_eq!(
            m.feed(InputEvent::press(kb(b'A' as u16))),
            MatchResult::PASS
        );
        assert_eq!(
            m.feed(InputEvent::release(kb(b'A' as u16))),
            MatchResult::PASS
        );
        assert_eq!(
            m.feed(InputEvent::release(F13)),
            MatchResult::PASS,
            "release without press"
        );
    }

    #[test]
    fn modifiers_are_strict() {
        let mut m = ChordMatcher::new(Chord::new(F13));
        m.feed(InputEvent::press(kb(vk::LCONTROL)));
        assert_eq!(
            m.feed(InputEvent::press(F13)),
            MatchResult::PASS,
            "Ctrl+F13 must not fire bare F13"
        );
        m.feed(InputEvent::release(F13));
        m.feed(InputEvent::release(kb(vk::LCONTROL)));
        assert_eq!(
            m.feed(InputEvent::press(F13)).event,
            Some(HotkeyEvent::Pressed)
        );
    }

    #[test]
    fn modifier_chord_requires_modifier_and_accepts_left_or_right() {
        let chord: Chord = "Ctrl+Shift+Space".parse().unwrap();
        let mut m = ChordMatcher::new(chord);

        assert_eq!(m.feed(InputEvent::press(kb(vk::SPACE))), MatchResult::PASS);
        m.feed(InputEvent::release(kb(vk::SPACE)));

        m.feed(InputEvent::press(kb(vk::RCONTROL)));
        m.feed(InputEvent::press(kb(vk::LSHIFT)));
        assert_eq!(
            m.feed(InputEvent::press(kb(vk::SPACE))).event,
            Some(HotkeyEvent::Pressed)
        );

        // Releasing a modifier early does not end the chord; releasing Space does.
        assert_eq!(
            m.feed(InputEvent::release(kb(vk::LSHIFT))),
            MatchResult::PASS
        );
        assert!(m.is_active());
        assert_eq!(
            m.feed(InputEvent::release(kb(vk::SPACE))).event,
            Some(HotkeyEvent::Released)
        );
        m.feed(InputEvent::release(kb(vk::RCONTROL)));
    }

    #[test]
    fn modifier_key_itself_can_be_the_chord() {
        let mut m = ChordMatcher::new(Chord::new(kb(vk::RCONTROL)));
        assert_eq!(
            m.feed(InputEvent::press(kb(vk::RCONTROL))).event,
            Some(HotkeyEvent::Pressed)
        );
        assert_eq!(
            m.feed(InputEvent::release(kb(vk::RCONTROL))).event,
            Some(HotkeyEvent::Released)
        );
    }

    #[test]
    fn mouse_button_chord() {
        let mut m = ChordMatcher::new(Chord::new(MOUSE4));
        assert_eq!(
            m.feed(InputEvent::press(MOUSE4)).event,
            Some(HotkeyEvent::Pressed)
        );
        assert_eq!(
            m.feed(InputEvent::press(Key::Mouse(MouseButton::Left))),
            MatchResult::PASS
        );
        assert_eq!(
            m.feed(InputEvent::release(MOUSE4)).event,
            Some(HotkeyEvent::Released)
        );
    }

    #[test]
    fn reset_clears_stuck_state() {
        let mut m = ChordMatcher::new(Chord::new(F13));
        m.feed(InputEvent::press(kb(vk::LCONTROL))); // release never observed
        m.reset();
        assert_eq!(
            m.feed(InputEvent::press(F13)).event,
            Some(HotkeyEvent::Pressed)
        );
    }

    #[test]
    fn set_chord_rebinds_and_resets() {
        let mut m = ChordMatcher::new(Chord::new(F13));
        m.feed(InputEvent::press(F13));
        m.set_chord(Chord::new(MOUSE4));
        assert!(!m.is_active());
        assert_eq!(m.feed(InputEvent::release(F13)), MatchResult::PASS);
        assert_eq!(
            m.feed(InputEvent::press(MOUSE4)).event,
            Some(HotkeyEvent::Pressed)
        );
    }
}
