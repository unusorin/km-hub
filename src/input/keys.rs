//! Key combo naming: the `keys = "ctrl+alt+kp1"` strings in `[[macro]]` and
//! what the capture tool prints. Pure data, shared by the config parser, the
//! hotkey FSM and `km-hub macro`.
//!
//! A combo is a set of side-agnostic modifiers (ctrl/alt/shift/super, either
//! side) plus exactly one trigger key, named as its evdev constant without the
//! `KEY_` prefix and lowercased: `kp1`, `f5`, `a`, `1`, `prog1`. Parsing is
//! case-insensitive; `Display` gives the canonical spelling, which is what
//! duplicates are compared by.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use evdev::KeyCode;

pub const CTRLS: [KeyCode; 2] = [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL];
pub const ALTS: [KeyCode; 2] = [KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT];
pub const SHIFTS: [KeyCode; 2] = [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT];
pub const SUPERS: [KeyCode; 2] = [KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Super,
}

impl Modifier {
    pub const ALL: [Modifier; 4] = [Modifier::Ctrl, Modifier::Alt, Modifier::Shift, Modifier::Super];

    /// The physical keys (left and right) that count as this modifier.
    pub fn keys(self) -> &'static [KeyCode; 2] {
        match self {
            Modifier::Ctrl => &CTRLS,
            Modifier::Alt => &ALTS,
            Modifier::Shift => &SHIFTS,
            Modifier::Super => &SUPERS,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "ctrl",
            Modifier::Alt => "alt",
            Modifier::Shift => "shift",
            Modifier::Super => "super",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "ctrl" | "control" => Some(Modifier::Ctrl),
            "alt" => Some(Modifier::Alt),
            "shift" => Some(Modifier::Shift),
            "super" | "meta" | "win" => Some(Modifier::Super),
            _ => None,
        }
    }
}

/// Which modifier a physical key is, if it is one.
pub fn modifier_of(key: KeyCode) -> Option<Modifier> {
    Modifier::ALL.into_iter().find(|m| m.keys().contains(&key))
}

/// A set of modifiers, side-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
}

impl Mods {
    pub fn is_empty(self) -> bool {
        !(self.ctrl || self.alt || self.shift || self.sup)
    }

    pub fn contains(self, m: Modifier) -> bool {
        match m {
            Modifier::Ctrl => self.ctrl,
            Modifier::Alt => self.alt,
            Modifier::Shift => self.shift,
            Modifier::Super => self.sup,
        }
    }

    pub fn set(&mut self, m: Modifier) {
        match m {
            Modifier::Ctrl => self.ctrl = true,
            Modifier::Alt => self.alt = true,
            Modifier::Shift => self.shift = true,
            Modifier::Super => self.sup = true,
        }
    }

    /// The modifiers among a set of held keys.
    pub fn from_held<'a>(held: impl IntoIterator<Item = &'a KeyCode>) -> Self {
        let mut mods = Mods::default();
        for key in held {
            if let Some(m) = modifier_of(*key) {
                mods.set(m);
            }
        }
        mods
    }
}

/// Modifiers plus one trigger key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub mods: Mods,
    pub key: KeyCode,
}

impl KeyCombo {
    /// Ctrl+Alt+F1..F12 with any extra modifiers belong to slot switching and
    /// re-pairing (the switch ignores Shift/Super rather than requiring their
    /// absence); a macro there would never fire.
    pub fn is_reserved(&self) -> bool {
        // KEY_F1..KEY_F10 are contiguous (59..68); F11/F12 sit at 87/88.
        let fkey = (KeyCode::KEY_F1.code()..=KeyCode::KEY_F10.code()).contains(&self.key.code())
            || matches!(self.key, KeyCode::KEY_F11 | KeyCode::KEY_F12);
        self.mods.ctrl && self.mods.alt && fkey
    }
}

/// `KEY_KP1` → `kp1`.
pub fn key_name(key: KeyCode) -> String {
    let debug = format!("{key:?}");
    debug
        .strip_prefix("KEY_")
        .unwrap_or(&debug)
        .to_ascii_lowercase()
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in Modifier::ALL {
            if self.mods.contains(m) {
                write!(f, "{}+", m.name())?;
            }
        }
        write!(f, "{}", key_name(self.key))
    }
}

impl FromStr for KeyCombo {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut mods = Mods::default();
        let mut trigger: Option<KeyCode> = None;
        for raw in s.split('+') {
            let token = raw.trim().to_ascii_lowercase();
            if token.is_empty() {
                bail!("empty key name in '{s}'");
            }
            if let Some(m) = Modifier::parse(&token) {
                if mods.contains(m) {
                    bail!("modifier '{token}' given twice in '{s}'");
                }
                mods.set(m);
                continue;
            }
            let Ok(key) = KeyCode::from_str(&format!("KEY_{}", token.to_ascii_uppercase())) else {
                bail!("unknown key '{token}' in '{s}' (evdev name without KEY_, e.g. kp1, f5, a)");
            };
            if modifier_of(key).is_some() {
                bail!(
                    "'{token}' in '{s}' is a modifier — write it as ctrl/alt/shift/super, \
                     the trigger must be another key"
                );
            }
            if trigger.is_some() {
                bail!("'{s}' needs exactly one key besides modifiers");
            }
            trigger = Some(key);
        }
        let Some(key) = trigger else {
            bail!("'{s}' needs a key besides modifiers");
        };
        Ok(KeyCombo { mods, key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> KeyCombo {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_and_prints_canonically() {
        let combo = parse("Alt + CTRL+KP1");
        assert_eq!(combo.key, KeyCode::KEY_KP1);
        assert!(combo.mods.ctrl && combo.mods.alt && !combo.mods.shift && !combo.mods.sup);
        assert_eq!(combo.to_string(), "ctrl+alt+kp1");
        assert_eq!(parse("shift+super+f13").to_string(), "shift+super+f13");
        assert_eq!(parse("A").to_string(), "a");
        assert_eq!(parse("1").to_string(), "1");
        assert_eq!(parse("ctrl+alt+shift+super+pause").to_string(), "ctrl+alt+shift+super+pause");
    }

    #[test]
    fn display_round_trips() {
        for s in ["ctrl+alt+kp1", "super+a", "prog1", "ctrl+alt+shift+super+f24"] {
            assert_eq!(parse(s).to_string(), s);
            assert_eq!(parse(&parse(s).to_string()), parse(s));
        }
    }

    #[test]
    fn modifier_aliases() {
        assert_eq!(parse("control+meta+x"), parse("ctrl+super+x"));
        assert_eq!(parse("win+x"), parse("super+x"));
    }

    #[test]
    fn rejects_bad_input() {
        for s in [
            "ctrl+bogus",
            "ctrl+alt",
            "ctrl+a+b",
            "ctrl+ctrl+a",
            "leftctrl+a",
            "ctrl+leftshift",
            "",
            "ctrl++a",
        ] {
            assert!(s.parse::<KeyCombo>().is_err(), "{s:?} should not parse");
        }
        let err = "ctrl+bogus".parse::<KeyCombo>().unwrap_err().to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn reserved_switch_combos() {
        assert!(parse("ctrl+alt+f1").is_reserved());
        assert!(parse("ctrl+alt+f12").is_reserved());
        assert!(parse("ctrl+alt+shift+f5").is_reserved());
        assert!(parse("ctrl+alt+super+f5").is_reserved());
        assert!(!parse("ctrl+alt+f13").is_reserved());
        assert!(!parse("ctrl+f5").is_reserved());
        assert!(!parse("alt+shift+f5").is_reserved());
        assert!(!parse("ctrl+alt+kp1").is_reserved());
    }

    #[test]
    fn mods_from_held_keys() {
        let held = [KeyCode::KEY_RIGHTCTRL, KeyCode::KEY_A, KeyCode::KEY_LEFTMETA];
        let mods = Mods::from_held(held.iter());
        assert!(mods.ctrl && mods.sup && !mods.alt && !mods.shift);
        assert_eq!(modifier_of(KeyCode::KEY_RIGHTSHIFT), Some(Modifier::Shift));
        assert_eq!(modifier_of(KeyCode::KEY_A), None);
    }
}
