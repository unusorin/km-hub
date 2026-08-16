//! HID SDP service record, published via BlueZ ProfileManager1.

use crate::hid;

/// Build the SDP record XML for a HID keyboard/mouse combo device.
/// PSMs: 0x11 control, 0x13 interrupt. Subclass 0xC0 = combo keyboard/pointing.
///
/// `0x0209` HIDBatteryPower is **true** even though the hub runs on mains, and
/// that is the point: no wireless keyboard or mouse in existence is mains
/// powered, so claiming otherwise opts us out of the link power management
/// every real HID device uses. An earlier revision set it false to keep hosts
/// from putting us in sniff mode, on the theory that sniff causes stutter. It
/// is the reverse under load — a sniff link holds reserved anchor points in the
/// host's schedule, while an active link is best-effort and is the first thing
/// starved when the host's radio gets busy. Declaring a battery is how we ask
/// for the reservation.
///
/// `0x020F`/`0x0210` are the SSR host parameters, in units of 0.625 ms: they
/// cap how far the host may subrate us rather than leaving the choice to
/// Apple's default. `MaxLatency` is the knob to loosen first if a host declines
/// SSR outright.
///
/// The SDP half only matters if the system knobs agree — adapter link policy
/// must permit sniff and the kernel's `idle_timeout` must be non-zero (see
/// `setup.sh`), and the L2CAP sockets must not force active mode (see
/// `transport::allow_sniff`).
pub fn hid_service_record(service_name: &str) -> String {
    let descriptor = hid::descriptor_hex();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
  <attribute id="0x0001">
    <sequence><uuid value="0x1124"/></sequence>
  </attribute>
  <attribute id="0x0004">
    <sequence>
      <sequence><uuid value="0x0100"/><uint16 value="0x0011"/></sequence>
      <sequence><uuid value="0x0011"/></sequence>
    </sequence>
  </attribute>
  <attribute id="0x0005">
    <sequence><uuid value="0x1002"/></sequence>
  </attribute>
  <attribute id="0x0006">
    <sequence>
      <uint16 value="0x656e"/><uint16 value="0x006a"/><uint16 value="0x0100"/>
    </sequence>
  </attribute>
  <attribute id="0x0009">
    <sequence>
      <sequence><uuid value="0x1124"/><uint16 value="0x0100"/></sequence>
    </sequence>
  </attribute>
  <attribute id="0x000d">
    <sequence>
      <sequence>
        <sequence><uuid value="0x0100"/><uint16 value="0x0013"/></sequence>
        <sequence><uuid value="0x0011"/></sequence>
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0100">
    <text value="{service_name}"/>
  </attribute>
  <attribute id="0x0101">
    <text value="Keyboard/mouse switcher"/>
  </attribute>
  <attribute id="0x0201">
    <uint16 value="0x0111"/>
  </attribute>
  <attribute id="0x0202">
    <uint8 value="0xc0"/>
  </attribute>
  <attribute id="0x0203">
    <uint8 value="0x00"/>
  </attribute>
  <attribute id="0x0204">
    <boolean value="true"/>
  </attribute>
  <attribute id="0x0205">
    <boolean value="true"/>
  </attribute>
  <attribute id="0x0206">
    <sequence>
      <sequence>
        <uint8 value="0x22"/>
        <text encoding="hex" value="{descriptor}"/>
      </sequence>
    </sequence>
  </attribute>
  <attribute id="0x0207">
    <sequence>
      <sequence><uint16 value="0x0409"/><uint16 value="0x0100"/></sequence>
    </sequence>
  </attribute>
  <attribute id="0x0209">
    <boolean value="true"/>
  </attribute>
  <attribute id="0x020a">
    <boolean value="true"/>
  </attribute>
  <attribute id="0x020c">
    <uint16 value="0x1f40"/>
  </attribute>
  <attribute id="0x020d">
    <boolean value="true"/>
  </attribute>
  <attribute id="0x020e">
    <boolean value="true"/>
  </attribute>
  <attribute id="0x020f">
    <uint16 value="0x0012"/>
  </attribute>
  <attribute id="0x0210">
    <uint16 value="0x0000"/>
  </attribute>
</record>
"#
    )
}
