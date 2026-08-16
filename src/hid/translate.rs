//! Stateful translation of evdev events into HID input reports.

use evdev::{KeyCode, RelativeAxisCode};

use super::HidFrame;

enum Usage {
    /// Bit index in the modifier byte (LeftControl = 0 .. Right GUI = 7).
    Modifier(u8),
    /// Regular HID keyboard usage code.
    Key(u8),
}

#[derive(Default)]
pub struct Translator {
    modifiers: u8,
    keys: [u8; 6],
    kbd_dirty: bool,
    buttons: u8,
    dx: i32,
    dy: i32,
    wheel: i32,
    /// A button changed since the last mouse report — must go out now.
    buttons_dirty: bool,
    /// Consumer usage currently held, 0 for none. The report is a one-entry
    /// array, so only the most recent of several held media keys is reported.
    consumer: u16,
    consumer_dirty: bool,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a key event (`value`: 0 = release, 1 = press, 2 = repeat).
    /// Repeats are ignored: HID hosts do their own key repeat.
    pub fn handle_key(&mut self, key: KeyCode, value: i32) {
        if value == 2 {
            return;
        }
        let pressed = value == 1;
        if let Some(usage) = consumer_usage(key) {
            // Releasing a key other than the one being reported (two media keys
            // held, first one let go) must not clear the live usage.
            let new = if pressed {
                usage
            } else if self.consumer == usage {
                0
            } else {
                self.consumer
            };
            if new != self.consumer {
                self.consumer = new;
                self.consumer_dirty = true;
            }
            return;
        }
        if let Some(bit) = mouse_button_bit(key) {
            let mask = 1 << bit;
            let new = if pressed { self.buttons | mask } else { self.buttons & !mask };
            if new != self.buttons {
                self.buttons = new;
                self.buttons_dirty = true;
            }
            return;
        }
        match key_usage(key) {
            Some(Usage::Modifier(bit)) => {
                let mask = 1 << bit;
                let new = if pressed { self.modifiers | mask } else { self.modifiers & !mask };
                if new != self.modifiers {
                    self.modifiers = new;
                    self.kbd_dirty = true;
                }
            }
            Some(Usage::Key(usage)) => {
                if pressed {
                    // >6 keys held: drop the extra key (no ErrorRollOver in the prototype)
                    if !self.keys.contains(&usage)
                        && let Some(slot) = self.keys.iter().position(|&k| k == 0)
                    {
                        self.keys[slot] = usage;
                        self.kbd_dirty = true;
                    }
                } else if let Some(slot) = self.keys.iter().position(|&k| k == usage) {
                    self.keys[slot] = 0;
                    // Compact so active usages stay at the front.
                    self.keys.sort_unstable_by_key(|&k| k == 0);
                    self.kbd_dirty = true;
                }
            }
            None => {}
        }
    }

    /// Feed a relative-axis event (mouse motion / wheel).
    pub fn handle_rel(&mut self, axis: RelativeAxisCode, value: i32) {
        match axis {
            RelativeAxisCode::REL_X => self.dx += value,
            RelativeAxisCode::REL_Y => self.dy += value,
            RelativeAxisCode::REL_WHEEL => self.wheel += value,
            _ => {}
        }
    }

    /// Emit all pending reports (keyboard, then mouse) — unpaced; the router
    /// uses the split variants below.
    #[cfg(test)]
    pub fn flush(&mut self) -> Vec<HidFrame> {
        let mut out = self.flush_keyboard();
        out.extend(self.flush_consumer());
        out.extend(self.flush_mouse());
        out
    }

    /// Emit the keyboard report if it changed.
    pub fn flush_keyboard(&mut self) -> Vec<HidFrame> {
        if !self.kbd_dirty {
            return Vec::new();
        }
        self.kbd_dirty = false;
        vec![self.keyboard_frame()]
    }

    /// Emit the consumer-control report if it changed. Never paced: media keys
    /// are discrete presses, so there is nothing to coalesce.
    pub fn flush_consumer(&mut self) -> Vec<HidFrame> {
        if !self.consumer_dirty {
            return Vec::new();
        }
        self.consumer_dirty = false;
        vec![self.consumer_frame()]
    }

    /// A mouse button changed since the last mouse report (must not wait).
    pub fn mouse_buttons_dirty(&self) -> bool {
        self.buttons_dirty
    }

    /// Unsent mouse motion is pending (may be paced).
    pub fn mouse_motion_pending(&self) -> bool {
        self.dx != 0 || self.dy != 0 || self.wheel != 0
    }

    /// Emit pending mouse reports. Accumulated deltas beyond the 8-bit report
    /// range are split across several frames instead of being clamped, so
    /// pacing motion never loses distance.
    pub fn flush_mouse(&mut self) -> Vec<HidFrame> {
        let mut out = Vec::new();
        if !self.buttons_dirty && !self.mouse_motion_pending() {
            return out;
        }
        loop {
            let (x, rx) = take_i8(self.dx);
            let (y, ry) = take_i8(self.dy);
            let (w, rw) = take_i8(self.wheel);
            out.push(HidFrame::Mouse([2, self.buttons, x as u8, y as u8, w as u8]));
            self.dx = rx;
            self.dy = ry;
            self.wheel = rw;
            if !self.mouse_motion_pending() {
                break;
            }
        }
        self.buttons_dirty = false;
        out
    }

    /// Release everything — sent to the old target when switching away so no
    /// key or button stays stuck.
    pub fn release_all(&mut self) -> Vec<HidFrame> {
        let mut out = Vec::new();
        if self.modifiers != 0 || self.keys.iter().any(|&k| k != 0) {
            self.modifiers = 0;
            self.keys = [0; 6];
            out.push(self.keyboard_frame());
        }
        if self.buttons != 0 {
            self.buttons = 0;
            out.push(HidFrame::Mouse([2, 0, 0, 0, 0]));
        }
        if self.consumer != 0 {
            self.consumer = 0;
            out.push(self.consumer_frame());
        }
        self.dx = 0;
        self.dy = 0;
        self.wheel = 0;
        self.kbd_dirty = false;
        self.buttons_dirty = false;
        self.consumer_dirty = false;
        out
    }

    fn keyboard_frame(&self) -> HidFrame {
        let k = &self.keys;
        HidFrame::Keyboard([1, self.modifiers, 0, k[0], k[1], k[2], k[3], k[4], k[5]])
    }

    fn consumer_frame(&self) -> HidFrame {
        let [lo, hi] = self.consumer.to_le_bytes();
        HidFrame::Consumer([4, lo, hi])
    }
}

/// Split `v` into a report-sized chunk and the remainder.
fn take_i8(v: i32) -> (i8, i32) {
    let c = v.clamp(-127, 127);
    (c as i8, v - c)
}

/// evdev button → bit in the mouse report's button byte, i.e. HID button usage
/// minus one. Usages 4 and 5 are what hosts read as back/forward: Linux
/// `hid-input` maps them straight back to BTN_SIDE/BTN_EXTRA, and macOS and
/// Windows treat them as the navigation pair. Mice disagree about which evdev
/// code the thumb buttons emit — Logitech sends BTN_SIDE/BTN_EXTRA, others send
/// BTN_BACK/BTN_FORWARD — so both spellings fold onto the same two bits.
fn mouse_button_bit(key: KeyCode) -> Option<u8> {
    match key {
        KeyCode::BTN_LEFT => Some(0),
        KeyCode::BTN_RIGHT => Some(1),
        KeyCode::BTN_MIDDLE => Some(2),
        KeyCode::BTN_SIDE | KeyCode::BTN_BACK => Some(3),
        KeyCode::BTN_EXTRA | KeyCode::BTN_FORWARD => Some(4),
        _ => None,
    }
}

/// evdev key code → HID usage on the consumer page (0x0C). Checked before the
/// keyboard page: some of these have keyboard-page equivalents that hosts treat
/// far less consistently.
fn consumer_usage(key: KeyCode) -> Option<u16> {
    use KeyCode as K;
    Some(match key {
        K::KEY_MUTE => 0xE2,
        K::KEY_VOLUMEUP => 0xE9,
        K::KEY_VOLUMEDOWN => 0xEA,
        K::KEY_PLAYPAUSE => 0xCD,
        K::KEY_STOPCD => 0xB7,
        K::KEY_NEXTSONG => 0xB5,
        K::KEY_PREVIOUSSONG => 0xB6,
        K::KEY_BRIGHTNESSUP => 0x6F,
        K::KEY_BRIGHTNESSDOWN => 0x70,
        _ => return None,
    })
}

/// evdev key code → HID usage (keyboard page 0x07).
fn key_usage(key: KeyCode) -> Option<Usage> {
    use KeyCode as K;
    use Usage::{Key, Modifier};
    let u = match key {
        K::KEY_LEFTCTRL => return Some(Modifier(0)),
        K::KEY_LEFTSHIFT => return Some(Modifier(1)),
        K::KEY_LEFTALT => return Some(Modifier(2)),
        K::KEY_LEFTMETA => return Some(Modifier(3)),
        K::KEY_RIGHTCTRL => return Some(Modifier(4)),
        K::KEY_RIGHTSHIFT => return Some(Modifier(5)),
        K::KEY_RIGHTALT => return Some(Modifier(6)),
        K::KEY_RIGHTMETA => return Some(Modifier(7)),

        K::KEY_A => 0x04,
        K::KEY_B => 0x05,
        K::KEY_C => 0x06,
        K::KEY_D => 0x07,
        K::KEY_E => 0x08,
        K::KEY_F => 0x09,
        K::KEY_G => 0x0A,
        K::KEY_H => 0x0B,
        K::KEY_I => 0x0C,
        K::KEY_J => 0x0D,
        K::KEY_K => 0x0E,
        K::KEY_L => 0x0F,
        K::KEY_M => 0x10,
        K::KEY_N => 0x11,
        K::KEY_O => 0x12,
        K::KEY_P => 0x13,
        K::KEY_Q => 0x14,
        K::KEY_R => 0x15,
        K::KEY_S => 0x16,
        K::KEY_T => 0x17,
        K::KEY_U => 0x18,
        K::KEY_V => 0x19,
        K::KEY_W => 0x1A,
        K::KEY_X => 0x1B,
        K::KEY_Y => 0x1C,
        K::KEY_Z => 0x1D,
        K::KEY_1 => 0x1E,
        K::KEY_2 => 0x1F,
        K::KEY_3 => 0x20,
        K::KEY_4 => 0x21,
        K::KEY_5 => 0x22,
        K::KEY_6 => 0x23,
        K::KEY_7 => 0x24,
        K::KEY_8 => 0x25,
        K::KEY_9 => 0x26,
        K::KEY_0 => 0x27,
        K::KEY_ENTER => 0x28,
        K::KEY_ESC => 0x29,
        K::KEY_BACKSPACE => 0x2A,
        K::KEY_TAB => 0x2B,
        K::KEY_SPACE => 0x2C,
        K::KEY_MINUS => 0x2D,
        K::KEY_EQUAL => 0x2E,
        K::KEY_LEFTBRACE => 0x2F,
        K::KEY_RIGHTBRACE => 0x30,
        K::KEY_BACKSLASH => 0x31,
        K::KEY_SEMICOLON => 0x33,
        K::KEY_APOSTROPHE => 0x34,
        K::KEY_GRAVE => 0x35,
        K::KEY_COMMA => 0x36,
        K::KEY_DOT => 0x37,
        K::KEY_SLASH => 0x38,
        K::KEY_CAPSLOCK => 0x39,
        K::KEY_F1 => 0x3A,
        K::KEY_F2 => 0x3B,
        K::KEY_F3 => 0x3C,
        K::KEY_F4 => 0x3D,
        K::KEY_F5 => 0x3E,
        K::KEY_F6 => 0x3F,
        K::KEY_F7 => 0x40,
        K::KEY_F8 => 0x41,
        K::KEY_F9 => 0x42,
        K::KEY_F10 => 0x43,
        K::KEY_F11 => 0x44,
        K::KEY_F12 => 0x45,
        K::KEY_SYSRQ => 0x46,
        K::KEY_SCROLLLOCK => 0x47,
        K::KEY_PAUSE => 0x48,
        K::KEY_INSERT => 0x49,
        K::KEY_HOME => 0x4A,
        K::KEY_PAGEUP => 0x4B,
        K::KEY_DELETE => 0x4C,
        K::KEY_END => 0x4D,
        K::KEY_PAGEDOWN => 0x4E,
        K::KEY_RIGHT => 0x4F,
        K::KEY_LEFT => 0x50,
        K::KEY_DOWN => 0x51,
        K::KEY_UP => 0x52,
        K::KEY_NUMLOCK => 0x53,
        K::KEY_KPSLASH => 0x54,
        K::KEY_KPASTERISK => 0x55,
        K::KEY_KPMINUS => 0x56,
        K::KEY_KPPLUS => 0x57,
        K::KEY_KPENTER => 0x58,
        K::KEY_KP1 => 0x59,
        K::KEY_KP2 => 0x5A,
        K::KEY_KP3 => 0x5B,
        K::KEY_KP4 => 0x5C,
        K::KEY_KP5 => 0x5D,
        K::KEY_KP6 => 0x5E,
        K::KEY_KP7 => 0x5F,
        K::KEY_KP8 => 0x60,
        K::KEY_KP9 => 0x61,
        K::KEY_KP0 => 0x62,
        K::KEY_KPDOT => 0x63,
        K::KEY_102ND => 0x64,
        K::KEY_COMPOSE => 0x65,
        _ => return None,
    };
    Some(Key(u))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn press_and_release_sequencing() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::KEY_A, 1);
        assert_eq!(
            t.flush(),
            vec![HidFrame::Keyboard([1, 0, 0, 0x04, 0, 0, 0, 0, 0])]
        );
        // No change between SYNs -> nothing emitted.
        assert!(t.flush().is_empty());
        t.handle_key(KeyCode::KEY_A, 0);
        assert_eq!(t.flush(), vec![HidFrame::Keyboard([1, 0, 0, 0, 0, 0, 0, 0, 0])]);
    }

    #[test]
    fn repeats_are_ignored() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::KEY_A, 1);
        t.flush();
        t.handle_key(KeyCode::KEY_A, 2);
        assert!(t.flush().is_empty());
    }

    #[test]
    fn modifier_bits() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::KEY_LEFTSHIFT, 1);
        t.handle_key(KeyCode::KEY_RIGHTMETA, 1);
        assert_eq!(
            t.flush(),
            vec![HidFrame::Keyboard([1, 0b1000_0010, 0, 0, 0, 0, 0, 0, 0])]
        );
    }

    #[test]
    fn rollover_overflow_drops_extras() {
        let mut t = Translator::new();
        for k in [
            KeyCode::KEY_A,
            KeyCode::KEY_B,
            KeyCode::KEY_C,
            KeyCode::KEY_D,
            KeyCode::KEY_E,
            KeyCode::KEY_F,
            KeyCode::KEY_G, // 7th key: dropped
        ] {
            t.handle_key(k, 1);
        }
        let frames = t.flush();
        assert_eq!(
            frames,
            vec![HidFrame::Keyboard([
                1, 0, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09
            ])]
        );
    }

    #[test]
    fn mouse_accumulates_and_splits() {
        let mut t = Translator::new();
        t.handle_rel(RelativeAxisCode::REL_X, 100);
        t.handle_rel(RelativeAxisCode::REL_X, 100);
        t.handle_rel(RelativeAxisCode::REL_Y, -3);
        t.handle_key(KeyCode::BTN_LEFT, 1);
        assert!(t.mouse_buttons_dirty() && t.mouse_motion_pending());
        let frames = t.flush();
        assert_eq!(
            frames,
            vec![
                HidFrame::Mouse([2, 0x01, 127, (-3i8) as u8, 0]),
                HidFrame::Mouse([2, 0x01, 73, 0, 0]),
            ]
        );
        assert!(!t.mouse_buttons_dirty() && !t.mouse_motion_pending());
        // Deltas reset after flush; button state persists.
        t.handle_rel(RelativeAxisCode::REL_WHEEL, 1);
        assert_eq!(t.flush(), vec![HidFrame::Mouse([2, 0x01, 0, 0, 1])]);
    }

    #[test]
    fn side_buttons_reach_the_report() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::BTN_SIDE, 1);
        assert_eq!(t.flush(), vec![HidFrame::Mouse([2, 0b0000_1000, 0, 0, 0])]);
        t.handle_key(KeyCode::BTN_EXTRA, 1);
        assert_eq!(t.flush(), vec![HidFrame::Mouse([2, 0b0001_1000, 0, 0, 0])]);
        t.handle_key(KeyCode::BTN_SIDE, 0);
        t.handle_key(KeyCode::BTN_EXTRA, 0);
        assert_eq!(t.flush(), vec![HidFrame::Mouse([2, 0, 0, 0, 0])]);
        // Mice that spell the thumb pair the other way land on the same bits.
        t.handle_key(KeyCode::BTN_BACK, 1);
        assert_eq!(t.flush(), vec![HidFrame::Mouse([2, 0b0000_1000, 0, 0, 0])]);
        t.handle_key(KeyCode::BTN_FORWARD, 1);
        assert_eq!(t.flush(), vec![HidFrame::Mouse([2, 0b0001_1000, 0, 0, 0])]);
    }

    #[test]
    fn media_keys_go_out_on_the_consumer_page() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::KEY_VOLUMEUP, 1);
        assert_eq!(t.flush(), vec![HidFrame::Consumer([4, 0xE9, 0])]);
        t.handle_key(KeyCode::KEY_VOLUMEUP, 0);
        assert_eq!(t.flush(), vec![HidFrame::Consumer([4, 0, 0])]);
        t.handle_key(KeyCode::KEY_VOLUMEDOWN, 1);
        assert_eq!(t.flush(), vec![HidFrame::Consumer([4, 0xEA, 0])]);
        // Letting go of an older key while a newer one is held must not clear
        // the report out from under the key still down.
        t.handle_key(KeyCode::KEY_MUTE, 1);
        assert_eq!(t.flush(), vec![HidFrame::Consumer([4, 0xE2, 0])]);
        t.handle_key(KeyCode::KEY_VOLUMEDOWN, 0);
        assert!(t.flush().is_empty());
        t.handle_key(KeyCode::KEY_MUTE, 0);
        assert_eq!(t.flush(), vec![HidFrame::Consumer([4, 0, 0])]);
        // Media keys must not also leak onto the keyboard page.
        t.handle_key(KeyCode::KEY_PLAYPAUSE, 1);
        assert_eq!(t.flush(), vec![HidFrame::Consumer([4, 0xCD, 0])]);
    }

    #[test]
    fn release_all_clears_a_held_media_key() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::KEY_VOLUMEUP, 1);
        t.flush();
        assert_eq!(t.release_all(), vec![HidFrame::Consumer([4, 0, 0])]);
        assert!(t.flush().is_empty());
    }

    #[test]
    fn release_all_clears_state() {
        let mut t = Translator::new();
        t.handle_key(KeyCode::KEY_A, 1);
        t.handle_key(KeyCode::KEY_LEFTCTRL, 1);
        t.handle_key(KeyCode::BTN_LEFT, 1);
        t.flush();
        let frames = t.release_all();
        assert_eq!(
            frames,
            vec![
                HidFrame::Keyboard([1, 0, 0, 0, 0, 0, 0, 0, 0]),
                HidFrame::Mouse([2, 0, 0, 0, 0]),
            ]
        );
        assert!(t.flush().is_empty());
    }
}
