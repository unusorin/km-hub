//! Pure hotkey state machine: Ctrl+Alt+F<n> switches to slot n, and adding
//! Shift switches *and* runs the slot's hooks. Configured macro combos
//! (`[[macro]]`, e.g. Ctrl+Alt+KP1) run a command on the hub without
//! switching, on any slot.
//!
//! Splitting the two is the point: moving the keyboard to a machine and
//! triggering the side effect that belongs to it (an HDMI input, a desk lamp)
//! are separate wishes, so the plain combo is deliberately silent.
//!
//! Combo keys are swallowed in both directions: the F-key (or macro trigger)
//! press/repeat/release never reaches any target, and the releases of the
//! modifiers that formed the combo are swallowed too (the router synthesizes
//! releases on the old target for keys it already forwarded).
//!
//! Macros match their modifier set exactly — Ctrl+Alt+KP1 does not fire while
//! Shift is also down — so combos differing only by a modifier can coexist.
//! Switch combos keep their subset rule (Shift is a flag on top of Ctrl+Alt).

use std::collections::{HashMap, HashSet};

use evdev::KeyCode;

use super::keys::{ALTS, CTRLS, KeyCombo, Modifier, Mods, SHIFTS};

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Forward the event to the active target.
    Pass,
    /// Drop the event entirely.
    Swallow,
    /// Switch to the given slot; the triggering event is swallowed.
    SwitchTo {
        slot: u8,
        /// Shift was held: also run the slot's hooks. One variant rather than
        /// two, so the router cannot forget to handle a flavour.
        fire_hooks: bool,
    },
    /// Run the macro at this index of the configured list; the triggering
    /// event is swallowed. `held_mods` are the modifier keys that formed the
    /// combo: their presses were already forwarded and their releases will be
    /// swallowed, so the router must release them on the target itself —
    /// there is no switch here to do it as a side effect.
    RunMacro { index: usize, held_mods: Vec<KeyCode> },
}

#[derive(Default)]
pub struct HotkeyFsm {
    /// Configured macro combos → index into the config's macro list.
    macros: HashMap<KeyCombo, usize>,
    /// Physical key state, tracked regardless of suppression.
    held: HashSet<KeyCode>,
    /// Keys whose remaining events (repeat/release) must be swallowed.
    suppressed: HashSet<KeyCode>,
    /// The combo F-key currently held down, and the slot it selected. The
    /// router times this to turn a long press into a re-pair request.
    combo_held: Option<(KeyCode, u8)>,
}

impl HotkeyFsm {
    /// `macros` in config order; `RunMacro { index }` refers to that order.
    /// Duplicates were rejected by the config parser, so later ones win here
    /// without ambiguity in practice.
    pub fn with_macros(macros: impl IntoIterator<Item = KeyCombo>) -> Self {
        Self {
            macros: macros.into_iter().enumerate().map(|(i, c)| (c, i)).collect(),
            ..Self::default()
        }
    }

    /// Feed a key event (`value`: 0 = release, 1 = press, 2 = repeat).
    pub fn on_key(&mut self, key: KeyCode, value: i32) -> Verdict {
        match value {
            1 => {
                self.held.insert(key);
                if let Some(slot) = fkey_slot(key)
                    && self.any_held(&CTRLS)
                    && self.any_held(&ALTS)
                {
                    // Read per press, never latched: letting go of Shift and
                    // pressing another F-key must be a silent switch again.
                    let fire_hooks = self.any_held(&SHIFTS);
                    // Swallow the F-key and the combo modifiers until released.
                    self.suppressed.insert(key);
                    for m in CTRLS.iter().chain(ALTS.iter()).chain(SHIFTS.iter()) {
                        if self.held.contains(m) {
                            self.suppressed.insert(*m);
                        }
                    }
                    self.combo_held = Some((key, slot));
                    return Verdict::SwitchTo { slot, fire_hooks };
                }
                if !self.macros.is_empty() {
                    let combo = KeyCombo { mods: Mods::from_held(&self.held), key };
                    if let Some(&index) = self.macros.get(&combo) {
                        // Same swallowing contract as the switch combo: the
                        // trigger and every held modifier stay ours until released.
                        self.suppressed.insert(key);
                        let mut held_mods = Vec::new();
                        for m in Modifier::ALL.iter().flat_map(|m| m.keys()) {
                            if self.held.contains(m) {
                                self.suppressed.insert(*m);
                                held_mods.push(*m);
                            }
                        }
                        return Verdict::RunMacro { index, held_mods };
                    }
                }
                if self.suppressed.contains(&key) {
                    Verdict::Swallow
                } else {
                    Verdict::Pass
                }
            }
            2 => {
                if self.suppressed.contains(&key) {
                    Verdict::Swallow
                } else {
                    Verdict::Pass
                }
            }
            0 => {
                self.held.remove(&key);
                if self.combo_held.is_some_and(|(combo, _)| combo == key) {
                    self.combo_held = None;
                }
                if self.suppressed.remove(&key) {
                    Verdict::Swallow
                } else {
                    Verdict::Pass
                }
            }
            _ => Verdict::Pass,
        }
    }

    /// The slot whose combo F-key is still physically held, if any. Holding it
    /// is what distinguishes "switch" from "re-pair this slot".
    pub fn combo_slot_held(&self) -> Option<u8> {
        self.combo_held.map(|(_, slot)| slot)
    }

    fn any_held(&self, keys: &[KeyCode]) -> bool {
        keys.iter().any(|k| self.held.contains(k))
    }
}

fn fkey_slot(key: KeyCode) -> Option<u8> {
    match key {
        KeyCode::KEY_F1 => Some(1),
        KeyCode::KEY_F2 => Some(2),
        KeyCode::KEY_F3 => Some(3),
        KeyCode::KEY_F4 => Some(4),
        KeyCode::KEY_F5 => Some(5),
        KeyCode::KEY_F6 => Some(6),
        KeyCode::KEY_F7 => Some(7),
        KeyCode::KEY_F8 => Some(8),
        KeyCode::KEY_F9 => Some(9),
        KeyCode::KEY_F10 => Some(10),
        KeyCode::KEY_F11 => Some(11),
        KeyCode::KEY_F12 => Some(12),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain switch: Ctrl+Alt+F<n> with no Shift.
    fn switch(slot: u8) -> Verdict {
        Verdict::SwitchTo { slot, fire_hooks: false }
    }

    /// A switch that also runs the slot's hooks: Ctrl+Alt+Shift+F<n>.
    fn switch_and_fire(slot: u8) -> Verdict {
        Verdict::SwitchTo { slot, fire_hooks: true }
    }

    #[test]
    fn plain_keys_pass() {
        let mut fsm = HotkeyFsm::default();
        assert_eq!(fsm.on_key(KeyCode::KEY_A, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_A, 2), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_A, 0), Verdict::Pass);
    }

    #[test]
    fn partial_combo_passes() {
        let mut fsm = HotkeyFsm::default();
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 1), Verdict::Pass);
        // F-key without Alt held is a normal key.
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Pass);
    }

    #[test]
    fn full_combo_switches_and_swallows() {
        let mut fsm = HotkeyFsm::default();
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTALT, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch(2));
        // Repeats and release of the F-key are swallowed.
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 2), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Swallow);
        // Modifier releases are swallowed (router synthesizes them on the old target).
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTALT, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Swallow);
        // Modifiers work normally again afterwards.
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Pass);
    }

    #[test]
    fn chained_switch_while_modifiers_held() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_RIGHTCTRL, 1);
        fsm.on_key(KeyCode::KEY_RIGHTALT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch(2));
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Swallow);
        // Modifiers still physically held: next F-key switches again.
        assert_eq!(fsm.on_key(KeyCode::KEY_F1, 1), switch(1));
        assert_eq!(fsm.on_key(KeyCode::KEY_F1, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_RIGHTALT, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_RIGHTCTRL, 0), Verdict::Swallow);
    }

    /// Pressing the same slot's combo again re-emits `SwitchTo`. With Shift
    /// that is the re-assert the router turns into a hook re-fire, so a side
    /// effect that didn't take can be retried without switching anywhere.
    /// Auto-repeat must NOT do this — only discrete presses.
    #[test]
    fn re_pressing_the_same_slot_emits_again_but_auto_repeat_does_not() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch(2));
        // Leaning on the key must not spam the router.
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 2), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 2), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Swallow);
        // A fresh press of the same F-key does, with the modifiers still down.
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch(2));
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Swallow);
        // And again after fully releasing and re-forming the combo.
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTALT, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Swallow);
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch(2));
    }

    #[test]
    fn combo_hold_is_visible_until_the_fkey_is_released() {
        let mut fsm = HotkeyFsm::default();
        assert_eq!(fsm.combo_slot_held(), None);
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F4, 1), switch(4));
        assert_eq!(fsm.combo_slot_held(), Some(4));
        // Auto-repeat while holding does not disturb it.
        fsm.on_key(KeyCode::KEY_F4, 2);
        assert_eq!(fsm.combo_slot_held(), Some(4));
        // Releasing a modifier first is still a held F-key.
        fsm.on_key(KeyCode::KEY_LEFTALT, 0);
        assert_eq!(fsm.combo_slot_held(), Some(4));
        fsm.on_key(KeyCode::KEY_F4, 0);
        assert_eq!(fsm.combo_slot_held(), None);
    }

    #[test]
    fn chaining_moves_the_hold_to_the_new_slot() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        fsm.on_key(KeyCode::KEY_F2, 1);
        assert_eq!(fsm.combo_slot_held(), Some(2));
        fsm.on_key(KeyCode::KEY_F3, 1);
        assert_eq!(fsm.combo_slot_held(), Some(3));
        // Releasing the earlier key must not clear the current hold.
        fsm.on_key(KeyCode::KEY_F2, 0);
        assert_eq!(fsm.combo_slot_held(), Some(3));
        fsm.on_key(KeyCode::KEY_F3, 0);
        assert_eq!(fsm.combo_slot_held(), None);
    }

    #[test]
    fn an_fkey_without_the_combo_is_not_a_hold() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), Verdict::Pass);
        assert_eq!(fsm.combo_slot_held(), None);
    }

    #[test]
    fn interleaved_keys_unaffected() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_X, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_F3, 1), switch(3));
        // Non-combo key keeps passing after the switch.
        assert_eq!(fsm.on_key(KeyCode::KEY_X, 0), Verdict::Pass);
    }

    /// Shift is what asks for the hooks, and it is swallowed like the other
    /// combo modifiers — a Shift release leaking to the target would land in
    /// whatever the user types next.
    #[test]
    fn shift_asks_for_the_hooks_and_is_swallowed() {
        let mut fsm = HotkeyFsm::default();
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTALT, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_F3, 1), switch_and_fire(3));
        assert_eq!(fsm.on_key(KeyCode::KEY_F3, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTALT, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Swallow);
        // Shift is an ordinary key again straight afterwards.
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_A, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_A, 0), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 0), Verdict::Pass);
    }

    #[test]
    fn either_shift_works() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_RIGHTCTRL, 1);
        fsm.on_key(KeyCode::KEY_RIGHTALT, 1);
        fsm.on_key(KeyCode::KEY_RIGHTSHIFT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F5, 1), switch_and_fire(5));
        assert_eq!(fsm.on_key(KeyCode::KEY_RIGHTSHIFT, 0), Verdict::Swallow);
    }

    /// The flag is read at the moment of the press, not latched: chaining from
    /// a hook switch to a silent one must not carry the hooks along.
    #[test]
    fn dropping_shift_makes_the_next_switch_silent_again() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch_and_fire(2));
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 0), Verdict::Swallow);
        // Ctrl+Alt still down, Shift gone: a plain switch.
        assert_eq!(fsm.on_key(KeyCode::KEY_F3, 1), switch(3));
        // And picking Shift back up asks for the hooks again.
        fsm.on_key(KeyCode::KEY_F3, 0);
        fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F4, 1), switch_and_fire(4));
    }

    /// Shift only means anything on top of the full combo; on its own it is a
    /// modifier like any other and Shift+F<n> stays an application shortcut.
    #[test]
    fn shift_alone_is_not_the_hotkey() {
        let mut fsm = HotkeyFsm::default();
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Pass);
        assert_eq!(fsm.combo_slot_held(), None);
        // Ctrl+Shift+F2 is still short of the combo.
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), Verdict::Pass);
        assert_eq!(fsm.combo_slot_held(), None);
    }

    /// The re-pair hold is about the held F-key, not about Shift: both combos
    /// arm it, so there is one rule to remember rather than two.
    #[test]
    fn the_shift_combo_arms_the_repair_hold_too() {
        let mut fsm = HotkeyFsm::default();
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch_and_fire(2));
        assert_eq!(fsm.combo_slot_held(), Some(2));
        fsm.on_key(KeyCode::KEY_F2, 0);
        assert_eq!(fsm.combo_slot_held(), None);
    }

    fn macro_fsm(combos: &[&str]) -> HotkeyFsm {
        HotkeyFsm::with_macros(combos.iter().map(|s| s.parse::<KeyCombo>().unwrap()))
    }

    /// The macro verdict, with the modifiers the router has to release.
    fn run_macro(index: usize, held_mods: &[KeyCode]) -> Verdict {
        Verdict::RunMacro { index, held_mods: held_mods.to_vec() }
    }

    #[test]
    fn macro_fires_and_swallows_like_a_switch() {
        let mut fsm = macro_fsm(&["ctrl+alt+kp1"]);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_RIGHTALT, 1), Verdict::Pass);
        assert_eq!(
            fsm.on_key(KeyCode::KEY_KP1, 1),
            run_macro(0, &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTALT])
        );
        // Leaning on the key does not re-fire; release is swallowed.
        assert_eq!(fsm.on_key(KeyCode::KEY_KP1, 2), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_KP1, 0), Verdict::Swallow);
        // A discrete re-press with the modifiers still down fires again.
        assert_eq!(
            fsm.on_key(KeyCode::KEY_KP1, 1),
            run_macro(0, &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTALT])
        );
        assert_eq!(fsm.on_key(KeyCode::KEY_KP1, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_RIGHTALT, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Swallow);
        // Modifiers are ordinary again.
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTCTRL, 0), Verdict::Pass);
        // No re-pair hold for macros.
        assert_eq!(fsm.combo_slot_held(), None);
    }

    /// Exact modifier match: an extra Shift is a different combo.
    #[test]
    fn macro_needs_the_exact_modifier_set() {
        let mut fsm = macro_fsm(&["ctrl+alt+kp1", "ctrl+alt+shift+kp2"]);
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_KP1, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_KP1, 0), Verdict::Pass);
        assert_eq!(
            fsm.on_key(KeyCode::KEY_KP2, 1),
            run_macro(1, &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTALT, KeyCode::KEY_LEFTSHIFT])
        );
        assert_eq!(fsm.on_key(KeyCode::KEY_KP2, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTSHIFT, 0), Verdict::Swallow);
        // Shift gone: now the first macro matches, the second does not.
        assert_eq!(fsm.on_key(KeyCode::KEY_KP2, 1), Verdict::Pass);
        assert_eq!(fsm.on_key(KeyCode::KEY_KP2, 0), Verdict::Pass);
        assert_eq!(
            fsm.on_key(KeyCode::KEY_KP1, 1),
            run_macro(0, &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTALT])
        );
        // Missing a modifier is a plain key.
        fsm.on_key(KeyCode::KEY_KP1, 0);
        fsm.on_key(KeyCode::KEY_LEFTALT, 0);
        assert_eq!(fsm.on_key(KeyCode::KEY_KP1, 1), Verdict::Pass);
    }

    #[test]
    fn bare_and_super_macros() {
        let mut fsm = macro_fsm(&["prog1", "super+kp3"]);
        // Nothing to release for a bare key.
        assert_eq!(fsm.on_key(KeyCode::KEY_PROG1, 1), run_macro(0, &[]));
        assert_eq!(fsm.on_key(KeyCode::KEY_PROG1, 0), Verdict::Swallow);
        // A bare macro does not fire with a modifier down.
        fsm.on_key(KeyCode::KEY_LEFTMETA, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_PROG1, 1), Verdict::Pass);
        fsm.on_key(KeyCode::KEY_PROG1, 0);
        assert_eq!(fsm.on_key(KeyCode::KEY_KP3, 1), run_macro(1, &[KeyCode::KEY_LEFTMETA]));
        assert_eq!(fsm.on_key(KeyCode::KEY_KP3, 0), Verdict::Swallow);
        assert_eq!(fsm.on_key(KeyCode::KEY_LEFTMETA, 0), Verdict::Swallow);
    }

    #[test]
    fn switch_combos_are_unaffected_by_macros() {
        let mut fsm = macro_fsm(&["ctrl+alt+kp1", "ctrl+alt+shift+a"]);
        fsm.on_key(KeyCode::KEY_LEFTCTRL, 1);
        fsm.on_key(KeyCode::KEY_LEFTALT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 1), switch(2));
        assert_eq!(fsm.on_key(KeyCode::KEY_F2, 0), Verdict::Swallow);
        assert!(matches!(fsm.on_key(KeyCode::KEY_KP1, 1), Verdict::RunMacro { index: 0, .. }));
        fsm.on_key(KeyCode::KEY_KP1, 0);
        fsm.on_key(KeyCode::KEY_LEFTSHIFT, 1);
        assert_eq!(fsm.on_key(KeyCode::KEY_F3, 1), switch_and_fire(3));
        assert!(matches!(fsm.on_key(KeyCode::KEY_A, 1), Verdict::RunMacro { index: 1, .. }));
        // Plain keys still pass with no macro configured for them.
        assert_eq!(fsm.on_key(KeyCode::KEY_X, 1), Verdict::Pass);
    }
}
