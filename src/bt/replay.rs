//! Frames held for a host that is between connections.
//!
//! In single-connection mode a slot switch disconnects the old host and waits
//! for the new one to reconnect and re-enable its input reports — about a
//! second on Linux, a little more on macOS, which re-reads the whole GATT
//! database first. Keys typed in that window would otherwise be dropped on
//! the floor (`LeTransport::send_to` has nobody to give them to), and a
//! dropped *release* is worse than a dropped press: the host keeps the key
//! down. So state-carrying reports (keys, media keys, mouse buttons) are
//! queued here and replayed, per report, as soon as the host subscribes to
//! that report; pointer motion is not — it is stale by the time it could go
//! out, and the next real movement supersedes it anyway.

use std::collections::VecDeque;

use crate::hid::HidFrame;

use super::gatt::is_motion_only;

/// Frames kept for one host. Enough for a burst of typing during a
/// reconnect; a stalled reconnect evicts the oldest (see `push`).
pub const CAPACITY: usize = 64;

/// What `push` did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// A mouse report that only moved the pointer: not worth replaying.
    DroppedMotion,
    /// Queued, but the buffer was full and its oldest frame is gone.
    Evicted,
}

#[derive(Debug, Default)]
pub struct ReplayBuffer {
    frames: VecDeque<HidFrame>,
    /// Buttons byte of the last mouse report queued (or replayed before the
    /// buffer existed: zero, i.e. none held), for `is_motion_only`.
    last_mouse_buttons: u8,
    dropped_motion: u32,
    evicted: u32,
}

impl ReplayBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a frame, unless it is pointer motion only. When full, the
    /// oldest frame goes — never the newest: the newest may be the release
    /// of a key whose press is already queued.
    pub fn push(&mut self, frame: HidFrame) -> Push {
        if is_motion_only(&frame, self.last_mouse_buttons) {
            self.dropped_motion += 1;
            return Push::DroppedMotion;
        }
        if let HidFrame::Mouse(b) = &frame {
            self.last_mouse_buttons = b[1];
        }
        let evicted = if self.frames.len() >= CAPACITY {
            self.frames.pop_front();
            self.evicted += 1;
            true
        } else {
            false
        };
        self.frames.push_back(frame);
        if evicted { Push::Evicted } else { Push::Queued }
    }

    /// Remove and return every queued frame with this report id, oldest
    /// first; the others stay for their own report's subscription.
    pub fn take(&mut self, report_id: u8) -> Vec<HidFrame> {
        let mut out = Vec::new();
        let mut rest = VecDeque::with_capacity(self.frames.len());
        for frame in self.frames.drain(..) {
            if frame.report_id() == report_id {
                out.push(frame);
            } else {
                rest.push_back(frame);
            }
        }
        self.frames = rest;
        out
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// (motion frames dropped, frames evicted) so far.
    pub fn stats(&self) -> (u32, u32) {
        (self.dropped_motion, self.evicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::gatt::{REPORT_ID_CONSUMER, REPORT_ID_KEYBOARD, REPORT_ID_MOUSE};

    fn key(usage: u8) -> HidFrame {
        HidFrame::Keyboard([REPORT_ID_KEYBOARD, 0, 0, usage, 0, 0, 0, 0, 0])
    }

    fn mouse(buttons: u8, dx: u8) -> HidFrame {
        HidFrame::Mouse([REPORT_ID_MOUSE, buttons, dx, 0, 0])
    }

    #[test]
    fn motion_is_dropped_but_button_changes_are_kept() {
        let mut b = ReplayBuffer::new();
        assert_eq!(b.push(mouse(0, 5)), Push::DroppedMotion);
        assert_eq!(b.push(mouse(1, 0)), Push::Queued); // press
        assert_eq!(b.push(mouse(1, 3)), Push::DroppedMotion); // drag
        assert_eq!(b.push(mouse(0, 0)), Push::Queued); // release
        assert_eq!(b.take(REPORT_ID_MOUSE), vec![mouse(1, 0), mouse(0, 0)]);
        assert_eq!(b.stats(), (2, 0));
    }

    #[test]
    fn keys_and_media_keys_are_kept_in_order() {
        let mut b = ReplayBuffer::new();
        b.push(key(0x04));
        b.push(HidFrame::Consumer([REPORT_ID_CONSUMER, 0xe9, 0]));
        b.push(key(0));
        assert_eq!(b.take(REPORT_ID_KEYBOARD), vec![key(0x04), key(0)]);
        // The consumer frame waits for its own report.
        assert_eq!(b.len(), 1);
        assert_eq!(b.take(REPORT_ID_CONSUMER), vec![HidFrame::Consumer([REPORT_ID_CONSUMER, 0xe9, 0])]);
        assert!(b.is_empty());
    }

    #[test]
    fn a_full_buffer_evicts_the_oldest() {
        let mut b = ReplayBuffer::new();
        for i in 0..CAPACITY {
            assert_eq!(b.push(key(i as u8)), Push::Queued);
        }
        assert_eq!(b.push(key(0xff)), Push::Evicted);
        let frames = b.take(REPORT_ID_KEYBOARD);
        assert_eq!(frames.len(), CAPACITY);
        assert_eq!(frames[0], key(1));
        assert_eq!(frames[CAPACITY - 1], key(0xff));
        assert_eq!(b.stats(), (0, 1));
    }

    #[test]
    fn take_of_an_unqueued_report_is_empty() {
        let mut b = ReplayBuffer::new();
        b.push(key(0x04));
        assert!(b.take(REPORT_ID_MOUSE).is_empty());
        assert_eq!(b.len(), 1);
    }
}
