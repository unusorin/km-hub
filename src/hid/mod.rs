//! HID report definitions shared by the translator and the Bluetooth transport.

pub mod translate;

/// Combined keyboard (report ID 1) + mouse (report ID 2) + consumer control
/// (report ID 4) report descriptor.
/// Keyboard: 1 modifier byte, 1 reserved, 6-key rollover array.
/// Mouse: 5 buttons, dx/dy/wheel as i8.
/// Consumer: one 16-bit usage code, 0 when nothing is held.
#[rustfmt::skip]
pub const REPORT_DESCRIPTOR: &[u8] = &[
    // Keyboard
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x06,       // Usage (Keyboard)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x01,       //   Report ID (1)
    0x05, 0x07,       //   Usage Page (Key Codes)
    0x19, 0xE0,       //   Usage Minimum (LeftControl)
    0x29, 0xE7,       //   Usage Maximum (Right GUI)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x01,       //   Logical Maximum (1)
    0x75, 0x01,       //   Report Size (1)
    0x95, 0x08,       //   Report Count (8)
    0x81, 0x02,       //   Input (Data, Variable, Absolute) — modifiers
    0x95, 0x01,       //   Report Count (1)
    0x75, 0x08,       //   Report Size (8)
    0x81, 0x01,       //   Input (Constant) — reserved
    0x95, 0x06,       //   Report Count (6)
    0x75, 0x08,       //   Report Size (8)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x65,       //   Logical Maximum (101)
    0x05, 0x07,       //   Usage Page (Key Codes)
    0x19, 0x00,       //   Usage Minimum (0)
    0x29, 0x65,       //   Usage Maximum (101)
    0x81, 0x00,       //   Input (Data, Array) — 6KRO keys
    // Battery level as a feature report, so macOS/iOS have something to poll.
    // Generic Device Controls / Battery Strength is the usage Apple's own
    // peripherals expose over BR/EDR HID; SDP attribute 0x0209 only claims a
    // battery exists, it carries no level. Report IDs are per-device, not
    // per-collection, so this rides in the keyboard collection rather than
    // adding a top-level one macOS would enumerate as a separate device.
    0x85, 0x03,       //   Report ID (3)
    0x05, 0x06,       //   Usage Page (Generic Device Controls)
    0x09, 0x20,       //   Usage (Battery Strength)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x64,       //   Logical Maximum (100)
    0x75, 0x08,       //   Report Size (8)
    0x95, 0x01,       //   Report Count (1)
    0xB1, 0x02,       //   Feature (Data, Variable, Absolute)
    0xC0,             // End Collection
    // Mouse
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x02,       // Usage (Mouse)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x02,       //   Report ID (2)
    0x09, 0x01,       //   Usage (Pointer)
    0xA1, 0x00,       //   Collection (Physical)
    0x05, 0x09,       //     Usage Page (Buttons)
    0x19, 0x01,       //     Usage Minimum (1)
    0x29, 0x05,       //     Usage Maximum (5)
    0x15, 0x00,       //     Logical Minimum (0)
    0x25, 0x01,       //     Logical Maximum (1)
    0x95, 0x05,       //     Report Count (5)
    0x75, 0x01,       //     Report Size (1)
    0x81, 0x02,       //     Input (Data, Variable, Absolute) — buttons
    0x95, 0x01,       //     Report Count (1)
    0x75, 0x03,       //     Report Size (3)
    0x81, 0x01,       //     Input (Constant) — padding
    0x05, 0x01,       //     Usage Page (Generic Desktop)
    0x09, 0x30,       //     Usage (X)
    0x09, 0x31,       //     Usage (Y)
    0x09, 0x38,       //     Usage (Wheel)
    0x15, 0x81,       //     Logical Minimum (-127)
    0x25, 0x7F,       //     Logical Maximum (127)
    0x75, 0x08,       //     Report Size (8)
    0x95, 0x03,       //     Report Count (3)
    0x81, 0x06,       //     Input (Data, Variable, Relative)
    0xC0,             //   End Collection
    0xC0,             // End Collection
    // Consumer control: volume and transport keys. A one-entry array carrying a
    // raw usage code, rather than a bitmap of named controls — it costs the same
    // two bytes on the wire and lets new keys be added by extending the match in
    // `translate`, with no descriptor change and so no re-pairing of every host.
    // The keyboard page has its own volume usages (0x7F..0x81), but macOS
    // ignores those on a HID keyboard; the consumer page is what hosts honour.
    0x05, 0x0C,       // Usage Page (Consumer)
    0x09, 0x01,       // Usage (Consumer Control)
    0xA1, 0x01,       // Collection (Application)
    0x85, 0x04,       //   Report ID (4)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x03, //   Logical Maximum (1023)
    0x19, 0x00,       //   Usage Minimum (0)
    0x2A, 0xFF, 0x03, //   Usage Maximum (1023)
    0x75, 0x10,       //   Report Size (16)
    0x95, 0x01,       //   Report Count (1)
    0x81, 0x00,       //   Input (Data, Array, Absolute)
    0xC0,             // End Collection
];

/// HIDP header prepended to every report on the interrupt channel:
/// message type DATA (0xA0) | report type Input (0x01).
pub const HIDP_DATA_INPUT: u8 = 0xA1;

/// Report ID of the battery feature report in [`REPORT_DESCRIPTOR`].
pub const BATTERY_REPORT_ID: u8 = 3;

/// Battery level answered for [`BATTERY_REPORT_ID`], in percent. Fixed: the hub
/// is mains powered and has no battery to measure. It reports one anyway
/// because every real wireless keyboard and mouse does, and a HID device that
/// declares no battery opts out of the host-side link power management they all
/// rely on — see the `0x0209` note in `bt::sdp`.
pub const BATTERY_LEVEL: u8 = 100;

/// A complete input report, report ID included as the first byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HidFrame {
    /// [report_id=1, modifiers, reserved, key1..key6]
    Keyboard([u8; 9]),
    /// [report_id=2, buttons, dx, dy, wheel]
    Mouse([u8; 5]),
    /// [report_id=4, usage_lo, usage_hi]
    Consumer([u8; 3]),
}

impl HidFrame {
    /// The whole report, ID first — what goes on the wire over classic HID.
    pub fn bytes(&self) -> &[u8] {
        match self {
            HidFrame::Keyboard(b) => b,
            HidFrame::Mouse(b) => b,
            HidFrame::Consumer(b) => b,
        }
    }

    /// The report ID, i.e. `bytes()[0]`.
    pub fn report_id(&self) -> u8 {
        self.bytes()[0]
    }

    /// The report without its ID — what a GATT Report characteristic carries,
    /// the ID living in its Report Reference descriptor instead.
    pub fn payload(&self) -> &[u8] {
        &self.bytes()[1..]
    }
}

/// Report descriptor as a lowercase hex string, for embedding in the SDP record.
pub fn descriptor_hex() -> String {
    REPORT_DESCRIPTOR
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Over GATT the report ID travels in the Report Reference descriptor, so
    /// the payload must be everything *after* it, for every report kind.
    #[test]
    fn payload_strips_report_id() {
        let kbd = HidFrame::Keyboard([1, 0x02, 0, 4, 5, 0, 0, 0, 0]);
        assert_eq!(kbd.report_id(), 1);
        assert_eq!(kbd.payload(), &[0x02, 0, 4, 5, 0, 0, 0, 0]);

        let mouse = HidFrame::Mouse([2, 0x01, 10, 0xF6, 0]);
        assert_eq!(mouse.report_id(), 2);
        assert_eq!(mouse.payload(), &[0x01, 10, 0xF6, 0]);

        let consumer = HidFrame::Consumer([4, 0xE9, 0x00]);
        assert_eq!(consumer.report_id(), 4);
        assert_eq!(consumer.payload(), &[0xE9, 0x00]);
    }
}
