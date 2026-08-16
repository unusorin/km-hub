//! OpenRGB SDK wire protocol, client side.
//!
//! Hand-rolled on purpose: both Rust SDK crates (`openrgb`, `openrgb2`) are
//! GPL-2.0 and km-hub is MIT. The format follows OpenRGB
//! `release_candidate_1.0rc3` — `NetworkProtocol.h` for the header and packet
//! IDs, `RGBController.cpp` for the device-description blob.
//!
//! km-hub asks for **protocol version 3**, the oldest version that still
//! carries mode brightness. The server serializes each controller at the
//! version named in the request (`NetworkServer::SendReply_ControllerData`), so
//! the blob never contains zone segments (v4+), zone flags or LED alternate
//! names (v5+) and the parser below stays small.

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const PROTOCOL_VERSION: u32 = 3;

const MAGIC: &[u8; 4] = b"ORGB";
const HEADER_LEN: usize = 16;

/// Guard against a desynced stream turning a bogus length into a huge
/// allocation. Real descriptions are a few KB.
const MAX_PACKET: u32 = 4 * 1024 * 1024;

pub mod pkt {
    pub const CONTROLLER_COUNT: u32 = 0;
    pub const CONTROLLER_DATA: u32 = 1;
    pub const PROTOCOL_VERSION: u32 = 40;
    pub const SET_CLIENT_NAME: u32 = 50;
    pub const DEVICE_LIST_UPDATED: u32 = 100;
    pub const RESCAN_DEVICES: u32 = 140;
    pub const UPDATE_LEDS: u32 = 1050;
    pub const UPDATE_MODE: u32 = 1101;
}

pub const MODE_FLAG_HAS_BRIGHTNESS: u32 = 1 << 4;
pub const MODE_FLAG_HAS_PER_LED_COLOR: u32 = 1 << 5;
pub const MODE_FLAG_HAS_MODE_SPECIFIC_COLOR: u32 = 1 << 6;

pub const MODE_COLORS_MODE_SPECIFIC: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const OFF: Rgb = Rgb { r: 0, g: 0, b: 0 };

    /// Accepts `#RRGGBB` or bare `RRGGBB`.
    pub fn parse(text: &str) -> Result<Self> {
        let hex = text.strip_prefix('#').unwrap_or(text);
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid color '{text}' (expected #RRGGBB)");
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digits checked");
        Ok(Self {
            r: byte(0),
            g: byte(2),
            b: byte(4),
        })
    }

    /// OpenRGB packs colors as `0x00BBGGRR`.
    pub fn to_wire(self) -> u32 {
        u32::from(self.r) | u32::from(self.g) << 8 | u32::from(self.b) << 16
    }
}

impl std::fmt::Display for Rgb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub dev_idx: u32,
    pub id: u32,
    pub data: Vec<u8>,
}

/// One device mode, kept whole so it can be sent back with only `brightness`
/// (and, for devices without per-LED color, `colors[0]`) changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    pub index: i32,
    pub name: String,
    pub value: i32,
    pub flags: u32,
    pub speed_min: u32,
    pub speed_max: u32,
    pub brightness_min: u32,
    pub brightness_max: u32,
    pub colors_min: u32,
    pub colors_max: u32,
    pub speed: u32,
    pub brightness: u32,
    pub direction: u32,
    pub color_mode: u32,
    pub colors: Vec<u32>,
}

impl Mode {
    pub fn has_brightness(&self) -> bool {
        self.flags & MODE_FLAG_HAS_BRIGHTNESS != 0
    }

    pub fn has_per_led_color(&self) -> bool {
        self.flags & MODE_FLAG_HAS_PER_LED_COLOR != 0
    }

    pub fn has_mode_specific_color(&self) -> bool {
        self.flags & MODE_FLAG_HAS_MODE_SPECIFIC_COLOR != 0 && self.colors_max > 0
    }

    /// `RGBController::GetModeDescription` at protocol 3.
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64);
        body.extend_from_slice(&self.index.to_le_bytes());
        put_string(&mut body, &self.name);
        body.extend_from_slice(&self.value.to_le_bytes());
        for v in [
            self.flags,
            self.speed_min,
            self.speed_max,
            self.brightness_min,
            self.brightness_max,
            self.colors_min,
            self.colors_max,
            self.speed,
            self.brightness,
            self.direction,
            self.color_mode,
        ] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&(self.colors.len() as u16).to_le_bytes());
        for color in &self.colors {
            body.extend_from_slice(&color.to_le_bytes());
        }
        with_size_prefix(body)
    }
}

#[derive(Debug, Clone)]
pub struct Device {
    pub index: u32,
    pub name: String,
    /// `device_type` enum value; kept for logging only.
    pub kind: i32,
    pub active_mode: i32,
    pub modes: Vec<Mode>,
    pub led_count: u16,
}

pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Packet> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).await?;
    if &header[..4] != MAGIC {
        bail!("bad packet magic {:?} — stream out of sync", &header[..4]);
    }
    let dev_idx = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let id = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let size = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if size > MAX_PACKET {
        bail!("packet {id} claims {size} bytes — refusing");
    }
    let mut data = vec![0u8; size as usize];
    reader.read_exact(&mut data).await?;
    Ok(Packet { dev_idx, id, data })
}

fn frame(dev_idx: u32, id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&dev_idx.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Payloads that carry their own length start with it, counting the field
/// itself (`GetColorDescription`, `GetModeDescription`).
fn with_size_prefix(body: Vec<u8>) -> Vec<u8> {
    let size = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Strings are a `u16` length followed by the bytes *including* the trailing
/// NUL, matching `strlen() + 1` on the C side.
fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&((text.len() + 1) as u16).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

pub fn req_client_name(name: &str) -> Vec<u8> {
    let mut data = name.as_bytes().to_vec();
    data.push(0);
    frame(0, pkt::SET_CLIENT_NAME, &data)
}

pub fn req_protocol_version() -> Vec<u8> {
    frame(0, pkt::PROTOCOL_VERSION, &PROTOCOL_VERSION.to_le_bytes())
}

pub fn req_controller_count() -> Vec<u8> {
    frame(0, pkt::CONTROLLER_COUNT, &[])
}

/// Ask the server to re-detect hardware. It only detects at startup, so a
/// device that appeared later (or that was not enumerated yet when the service
/// started) stays invisible until something asks for this.
pub fn req_rescan_devices() -> Vec<u8> {
    frame(0, pkt::RESCAN_DEVICES, &[])
}

/// The server serializes the description at the version sent here, so this is
/// what actually pins the blob layout to v3.
pub fn req_controller_data(dev_idx: u32) -> Vec<u8> {
    frame(
        dev_idx,
        pkt::CONTROLLER_DATA,
        &PROTOCOL_VERSION.to_le_bytes(),
    )
}

pub fn update_leds(dev_idx: u32, color: Rgb, led_count: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + 4 * usize::from(led_count));
    body.extend_from_slice(&led_count.to_le_bytes());
    let wire = color.to_wire().to_le_bytes();
    for _ in 0..led_count {
        body.extend_from_slice(&wire);
    }
    frame(dev_idx, pkt::UPDATE_LEDS, &with_size_prefix(body))
}

pub fn update_mode(dev_idx: u32, mode: &Mode) -> Vec<u8> {
    frame(dev_idx, pkt::UPDATE_MODE, &mode.encode())
}

/// Cursor over a device description, bounds-checked so a short or malformed
/// blob is an error instead of a panic.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(end) => {
                let slice = &self.buf[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => bail!(
                "device description truncated: want {n} bytes at {} of {}",
                self.pos,
                self.buf.len()
            ),
        }
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// Length-prefixed string; the length counts the trailing NUL.
    fn string(&mut self) -> Result<String> {
        let len = usize::from(self.u16()?);
        let raw = self.take(len)?;
        let text = raw.split(|&b| b == 0).next().unwrap_or(raw);
        Ok(String::from_utf8_lossy(text).into_owned())
    }

    fn skip_string(&mut self) -> Result<()> {
        let len = usize::from(self.u16()?);
        self.take(len).map(|_| ())
    }
}

/// `RGBController::ReadDeviceDescription` at protocol 3.
pub fn parse_device(index: u32, data: &[u8]) -> Result<Device> {
    let mut cur = Cursor::new(data);
    cur.u32()?; // total size, already implied by the packet length
    let kind = cur.i32()?;
    let name = cur.string()?;
    for _ in 0..5 {
        // vendor (protocol >= 1), description, version, serial, location
        cur.skip_string()?;
    }

    let num_modes = cur.u16()?;
    let active_mode = cur.i32()?;
    let mut modes = Vec::with_capacity(usize::from(num_modes));
    for index in 0..num_modes {
        let name = cur.string()?;
        let value = cur.i32()?;
        let flags = cur.u32()?;
        let speed_min = cur.u32()?;
        let speed_max = cur.u32()?;
        let brightness_min = cur.u32()?;
        let brightness_max = cur.u32()?;
        let colors_min = cur.u32()?;
        let colors_max = cur.u32()?;
        let speed = cur.u32()?;
        let brightness = cur.u32()?;
        let direction = cur.u32()?;
        let color_mode = cur.u32()?;
        let num_colors = cur.u16()?;
        let mut colors = Vec::with_capacity(usize::from(num_colors));
        for _ in 0..num_colors {
            colors.push(cur.u32()?);
        }
        modes.push(Mode {
            index: i32::from(index),
            name,
            value,
            flags,
            speed_min,
            speed_max,
            brightness_min,
            brightness_max,
            colors_min,
            colors_max,
            speed,
            brightness,
            direction,
            color_mode,
            colors,
        });
    }

    // Zones are skipped, but must be walked to reach the LED count.
    let num_zones = cur.u16()?;
    for _ in 0..num_zones {
        cur.skip_string()?;
        cur.i32()?; // type
        cur.u32()?; // leds_min
        cur.u32()?; // leds_max
        cur.u32()?; // leds_count
        let matrix_len = cur.u16()?;
        if matrix_len > 0 {
            let height = cur.u32()?;
            let width = cur.u32()?;
            let cells = usize::try_from(height)
                .ok()
                .zip(usize::try_from(width).ok())
                .and_then(|(h, w)| h.checked_mul(w))
                .and_then(|cells| cells.checked_mul(4))
                .ok_or_else(|| anyhow::anyhow!("absurd matrix map {height}x{width}"))?;
            cur.take(cells)?;
        }
    }

    let led_count = cur.u16()?;
    Ok(Device {
        index,
        name,
        kind,
        active_mode,
        modes,
        led_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Rgb {
        Rgb {
            r: 0xff,
            g: 0,
            b: 0,
        }
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(Rgb::parse("#ff0000").unwrap(), red());
        assert_eq!(Rgb::parse("FF0000").unwrap(), red());
        assert_eq!(
            Rgb::parse("#0a141e").unwrap(),
            Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        );
        for bad in ["#ff00", "#ff00000", "#gg0000", "", "red"] {
            assert!(Rgb::parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn color_wire_order_is_bgr() {
        // 0x00BBGGRR
        assert_eq!(red().to_wire(), 0x0000_00ff);
        assert_eq!(Rgb { r: 0, g: 0, b: 0xff }.to_wire(), 0x00ff_0000);
        assert_eq!(
            Rgb {
                r: 0x11,
                g: 0x22,
                b: 0x33
            }
            .to_wire(),
            0x0033_2211
        );
    }

    #[test]
    fn update_leds_payload_is_byte_exact() {
        let packet = update_leds(2, red(), 3);
        assert_eq!(&packet[..4], MAGIC);
        assert_eq!(u32::from_le_bytes(packet[4..8].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(packet[8..12].try_into().unwrap()),
            pkt::UPDATE_LEDS
        );
        let size = u32::from_le_bytes(packet[12..16].try_into().unwrap()) as usize;
        let body = &packet[16..];
        assert_eq!(size, body.len());
        // size prefix (4) + count (2) + 3 colors (12)
        assert_eq!(body.len(), 18);
        assert_eq!(u32::from_le_bytes(body[..4].try_into().unwrap()), 18);
        assert_eq!(u16::from_le_bytes(body[4..6].try_into().unwrap()), 3);
        for chunk in body[6..].chunks(4) {
            assert_eq!(u32::from_le_bytes(chunk.try_into().unwrap()), 0xff);
        }
    }

    fn sample_mode(index: i32, name: &str, flags: u32) -> Mode {
        Mode {
            index,
            name: name.into(),
            value: 7,
            flags,
            speed_min: 1,
            speed_max: 9,
            brightness_min: 0,
            brightness_max: 100,
            colors_min: 0,
            colors_max: 1,
            speed: 5,
            brightness: 42,
            direction: 0,
            color_mode: 1,
            colors: vec![red().to_wire()],
        }
    }

    /// Encode a mode the way the server would inside a device description, so
    /// the parser and the encoder are checked against each other.
    fn push_mode(out: &mut Vec<u8>, mode: &Mode) {
        put_string(out, &mode.name);
        out.extend_from_slice(&mode.value.to_le_bytes());
        for v in [
            mode.flags,
            mode.speed_min,
            mode.speed_max,
            mode.brightness_min,
            mode.brightness_max,
            mode.colors_min,
            mode.colors_max,
            mode.speed,
            mode.brightness,
            mode.direction,
            mode.color_mode,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(mode.colors.len() as u16).to_le_bytes());
        for c in &mode.colors {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }

    fn sample_description(modes: &[Mode], zone_leds: &[(u32, Option<(u32, u32)>)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1i32.to_le_bytes()); // device type
        put_string(&mut body, "SteelSeries Apex 3");
        for text in ["SteelSeries", "desc", "1.0", "serial", "location"] {
            put_string(&mut body, text);
        }
        body.extend_from_slice(&(modes.len() as u16).to_le_bytes());
        body.extend_from_slice(&0i32.to_le_bytes()); // active mode
        for mode in modes {
            push_mode(&mut body, mode);
        }
        body.extend_from_slice(&(zone_leds.len() as u16).to_le_bytes());
        let mut total_leds = 0u16;
        for (leds, matrix) in zone_leds {
            put_string(&mut body, "zone");
            body.extend_from_slice(&1i32.to_le_bytes()); // zone type
            body.extend_from_slice(&0u32.to_le_bytes()); // leds_min
            body.extend_from_slice(&leds.to_le_bytes()); // leds_max
            body.extend_from_slice(&leds.to_le_bytes()); // leds_count
            match matrix {
                Some((h, w)) => {
                    body.extend_from_slice(&((h * w * 4) as u16).to_le_bytes());
                    body.extend_from_slice(&h.to_le_bytes());
                    body.extend_from_slice(&w.to_le_bytes());
                    body.extend(std::iter::repeat_n(0u8, (h * w * 4) as usize));
                }
                None => body.extend_from_slice(&0u16.to_le_bytes()),
            }
            total_leds += *leds as u16;
        }
        body.extend_from_slice(&total_leds.to_le_bytes());
        for _ in 0..total_leds {
            put_string(&mut body, "led");
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        body.extend_from_slice(&total_leds.to_le_bytes());
        for _ in 0..total_leds {
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        with_size_prefix(body)
    }

    #[test]
    fn parses_a_v3_device_description() {
        let modes = vec![
            sample_mode(0, "Static", MODE_FLAG_HAS_BRIGHTNESS),
            sample_mode(
                1,
                "Direct",
                MODE_FLAG_HAS_BRIGHTNESS | MODE_FLAG_HAS_PER_LED_COLOR,
            ),
        ];
        let blob = sample_description(&modes, &[(4, None), (6, Some((2, 3)))]);
        let device = parse_device(3, &blob).unwrap();
        assert_eq!(device.index, 3);
        assert_eq!(device.name, "SteelSeries Apex 3");
        assert_eq!(device.led_count, 10);
        assert_eq!(device.active_mode, 0);
        assert_eq!(device.modes, modes);
        assert!(device.modes[1].has_per_led_color());
        assert!(!device.modes[0].has_per_led_color());
    }

    #[test]
    fn mode_encode_round_trips_through_the_parser() {
        let mut mode = sample_mode(0, "Direct", MODE_FLAG_HAS_BRIGHTNESS);
        mode.brightness = 77;
        let blob = sample_description(std::slice::from_ref(&mode), &[(1, None)]);
        let parsed = parse_device(0, &blob).unwrap().modes.remove(0);
        assert_eq!(parsed, mode);

        // The UpdateMode payload carries the same fields, prefixed by its size.
        let packet = update_mode(1, &parsed);
        let body = &packet[16..];
        assert_eq!(
            u32::from_le_bytes(body[..4].try_into().unwrap()) as usize,
            body.len()
        );
        assert_eq!(i32::from_le_bytes(body[4..8].try_into().unwrap()), 0);
    }

    #[test]
    fn truncated_description_errors_instead_of_panicking() {
        let blob = sample_description(&[sample_mode(0, "Direct", 0)], &[(2, None)]);
        let needed = (0..=blob.len())
            .find(|&cut| parse_device(0, &blob[..cut]).is_ok())
            .expect("the whole blob parses");
        // Anything short of what the parser reads is an error, never a panic.
        for cut in 0..needed {
            assert!(parse_device(0, &blob[..cut]).is_err(), "cut at {cut}");
        }
        // Parsing stops at the LED count; the LED and color arrays after it are
        // skipped, so they are not required to be present.
        assert!(needed < blob.len());
        assert_eq!(parse_device(0, &blob[..needed]).unwrap().led_count, 2);
    }

    #[tokio::test]
    async fn reads_a_framed_packet() {
        let bytes = req_controller_data(5);
        let mut cursor = std::io::Cursor::new(bytes);
        let packet = read_packet(&mut cursor).await.unwrap();
        assert_eq!(packet.dev_idx, 5);
        assert_eq!(packet.id, pkt::CONTROLLER_DATA);
        assert_eq!(
            u32::from_le_bytes(packet.data[..].try_into().unwrap()),
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn rescan_request_is_a_bare_header() {
        let packet = req_rescan_devices();
        assert_eq!(packet.len(), HEADER_LEN);
        assert_eq!(
            u32::from_le_bytes(packet[8..12].try_into().unwrap()),
            pkt::RESCAN_DEVICES
        );
        assert_eq!(u32::from_le_bytes(packet[12..16].try_into().unwrap()), 0);
    }

    #[tokio::test]
    async fn rejects_a_bad_magic() {
        let mut bytes = req_controller_count();
        bytes[0] = b'X';
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(read_packet(&mut cursor).await.is_err());
    }
}
