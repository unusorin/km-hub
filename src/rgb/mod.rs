//! Slot lighting: drives the hub's own keyboard/mouse LEDs through a local
//! OpenRGB SDK server so the active slot is visible on the desk.
//!
//! Strictly cosmetic. The task owns its socket, never touches the router or the
//! Bluetooth side, and treats every failure as "no lighting for now" — a dead
//! or missing OpenRGB server can neither delay nor block a switch.

pub mod proto;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::RgbSettings;
pub use proto::Rgb;
use proto::{Device, Mode, Packet};

const CLIENT_NAME: &str = "km-hub";
/// A reply the server never sends must not wedge the task.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Half a period: on/off at 500 ms is a 1 Hz blink.
const BLINK: Duration = Duration::from_millis(500);
const FLASH: Duration = Duration::from_millis(180);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Idle fade frame interval. 25 fps is smooth to the eye and nothing at all
/// for an OpenRGB server over loopback.
const FADE_TICK: Duration = Duration::from_millis(40);
/// Waking is quick but not instant — a snap reads as a glitch, a short ramp
/// reads as deliberate. Going dark is slow enough to be a sigh rather than a
/// switch flicking off.
const FADE_IN: Duration = Duration::from_millis(250);
const FADE_OUT: Duration = Duration::from_millis(1000);
/// Level added or removed per [`FADE_TICK`], rounded up so a fade always
/// reaches its endpoint rather than stalling a step short. The durations above
/// stay the source of truth; the rounding costs ~30 ms on each.
const FADE_IN_STEP: u8 = fade_step(FADE_IN);
const FADE_OUT_STEP: u8 = fade_step(FADE_OUT);
/// The router pings at most this often while you type. Long enough to keep a
/// 1000 Hz mouse from flooding the channel, short enough that the first event
/// after any real pause is always sent immediately — so waking is never
/// delayed by the coalescing.
const ACTIVITY_COALESCE: Duration = Duration::from_secs(1);

/// OpenRGB only detects hardware when it starts. If it came up before the USB
/// devices were enumerated (a boot race) it has nothing to drive, so ask it to
/// look again — but only while we know of no devices at all, never as a poll
/// against working hardware.
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);

const fn fade_step(total: Duration) -> u8 {
    let (tick, total) = (FADE_TICK.as_millis(), total.as_millis());
    let step = (255 * tick).div_ceil(total);
    if step > 255 { 255 } else { step as u8 }
}

/// Coalesced "the user is here" signal from the router to the lighting task.
///
/// Lives here rather than in the router because idling the LEDs is a lighting
/// concern; the router only knows it saw an event.
pub struct Activity {
    tx: watch::Sender<Instant>,
    last_sent: Instant,
}

impl Activity {
    pub fn new(tx: watch::Sender<Instant>) -> Self {
        Self {
            // Backdated so the very first event pings without waiting.
            last_sent: Instant::now() - ACTIVITY_COALESCE,
            tx,
        }
    }

    /// Called for every input event, so it must stay cheap. A send with no
    /// receiver left (the lighting task ended) is ignored like every other
    /// lighting failure.
    pub fn ping(&mut self, now: Instant) {
        if now < self.last_sent + ACTIVITY_COALESCE {
            return;
        }
        self.last_sent = now;
        let _ = self.tx.send(now);
    }
}

/// Bluetooth-side happenings the lighting reflects. Sent with `try_send`: a
/// full channel drops the event rather than stalling the Bluetooth task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgbEvent {
    PairingOpen,
    PairingClosed,
    Bound,
    /// Re-read the device list and push every color and mode again from
    /// scratch. The user's retry for lighting that came out wrong — a device
    /// that took half an update leaves the keyboard half one color, half
    /// another, and nothing else would ever correct it.
    Repaint,
}

pub struct RgbDeps {
    pub settings: RgbSettings,
    pub slot_rx: watch::Receiver<u8>,
    /// Last time the router saw input, coalesced by [`Activity`].
    pub activity_rx: watch::Receiver<Instant>,
    pub event_rx: mpsc::Receiver<RgbEvent>,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    color: Rgb,
    brightness: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anim {
    Steady,
    Pairing { lit: bool },
    /// Two green flashes on a successful bind, then back to the slot color.
    Flash { left: u8, lit: bool },
}

/// Scale a color toward black. The curve is squared because the eye's response
/// to light is not linear: scaled straight, a fade rushes the bright end and
/// crawls near black instead of reading as an even ramp.
fn dim(color: Rgb, level: u8) -> Rgb {
    if level == u8::MAX {
        return color;
    }
    let factor = u32::from(level) * u32::from(level);
    let scale = |c: u8| ((u32::from(c) * factor) / (255 * 255)) as u8;
    Rgb {
        r: scale(color.r),
        g: scale(color.g),
        b: scale(color.b),
    }
}

/// What the LEDs should show right now, independent of whether a server is
/// reachable to show it.
struct Lights {
    settings: RgbSettings,
    slot: u8,
    anim: Anim,
    anim_tick: Option<Instant>,
    /// Idle gate, 255 lit down to 0 dark. Applied over whatever `anim` renders,
    /// so an animation keeps running behind a dark screen and is showing the
    /// right thing the moment the user comes back.
    level: u8,
    /// The idle deadline has passed: the gate wants black. Held separately from
    /// `level` so an interrupted fade reverses from where it is.
    dark: bool,
    /// When the LEDs go dark. `None` while already dark, or when blanking is
    /// switched off entirely.
    idle_at: Option<Instant>,
    fade_tick: Option<Instant>,
}

impl Lights {
    fn new(settings: RgbSettings, slot: u8, now: Instant) -> Self {
        Self {
            // Lit at startup, dark once the timeout elapses with nobody there:
            // a hub that just came back up says so.
            idle_at: settings.idle_timeout.map(|timeout| now + timeout),
            settings,
            slot,
            anim: Anim::Steady,
            anim_tick: None,
            level: u8::MAX,
            dark: false,
            fade_tick: None,
        }
    }

    /// The earliest of the three independent deadlines, so `run` needs only one
    /// timer arm.
    fn next_tick(&self) -> Option<Instant> {
        [self.anim_tick, self.fade_tick, self.idle_at]
            .into_iter()
            .flatten()
            .min()
    }

    fn frame(&self) -> Frame {
        let frame = match self.anim {
            Anim::Steady => Frame {
                color: self.settings.slot_color(self.slot),
                brightness: self.settings.slot_brightness(self.slot),
            },
            // Blinks keep the slot's brightness so only the color packet
            // changes between frames.
            Anim::Pairing { lit } => self.blink(lit, self.settings.pairing_color),
            Anim::Flash { lit, .. } => self.blink(lit, self.settings.bound_color),
        };
        // The idle gate rides on the color for the same reason: most mice have
        // no brightness control at all (see `Conn::apply`), so dimming that way
        // would fade the keyboard and snap the mouse.
        Frame {
            color: dim(frame.color, self.level),
            ..frame
        }
    }

    fn blink(&self, lit: bool, color: Rgb) -> Frame {
        Frame {
            color: if lit { color } else { Rgb::OFF },
            brightness: self.settings.slot_brightness(self.slot),
        }
    }

    fn set_slot(&mut self, slot: u8) {
        self.slot = slot;
    }

    fn on_event(&mut self, event: RgbEvent, now: Instant) {
        match event {
            RgbEvent::PairingOpen => {
                self.anim = Anim::Pairing { lit: true };
                self.anim_tick = Some(now + BLINK);
            }
            RgbEvent::PairingClosed => {
                if matches!(self.anim, Anim::Pairing { .. }) {
                    self.anim = Anim::Steady;
                    self.anim_tick = None;
                }
            }
            RgbEvent::Bound => {
                self.anim = Anim::Flash {
                    left: 3,
                    lit: true,
                };
                self.anim_tick = Some(now + FLASH);
            }
            // Intercepted by `run`: it repaints the hardware and changes
            // nothing about what we are trying to show.
            RgbEvent::Repaint => {}
        }
    }

    /// The user touched the keyboard or the mouse: restart the idle countdown
    /// and bring the LEDs back up.
    fn on_activity(&mut self, at: Instant, now: Instant) {
        if self.dark {
            debug!("input — lighting up");
        }
        self.dark = false;
        self.idle_at = self.settings.idle_timeout.map(|timeout| at + timeout);
        self.sync_fade(now);
    }

    fn tick(&mut self, now: Instant) {
        if self.idle_at.is_some_and(|at| now >= at) {
            debug!(
                timeout_secs = self.settings.idle_timeout.unwrap_or_default().as_secs(),
                "no input — fading the lighting out"
            );
            self.dark = true;
            self.idle_at = None;
            self.sync_fade(now);
        }
        if self.fade_tick.is_some_and(|at| now >= at) {
            self.step_fade(now);
        }
        if self.anim_tick.is_some_and(|at| now >= at) {
            self.tick_anim(now);
        }
    }

    /// Arm the fade timer if the level is not already where the gate wants it.
    fn sync_fade(&mut self, now: Instant) {
        let settled = self.level == if self.dark { 0 } else { u8::MAX };
        self.fade_tick = (!settled).then_some(now + FADE_TICK);
    }

    /// One fade frame. Stepping from the *current* level is what makes an
    /// interrupted fade reverse smoothly: catch it half dark and it takes half
    /// the time to come back, with no jump.
    fn step_fade(&mut self, now: Instant) {
        self.level = if self.dark {
            self.level.saturating_sub(FADE_OUT_STEP)
        } else {
            self.level.saturating_add(FADE_IN_STEP)
        };
        self.sync_fade(now);
    }

    fn tick_anim(&mut self, now: Instant) {
        match &mut self.anim {
            Anim::Steady => self.anim_tick = None,
            Anim::Pairing { lit } => {
                *lit = !*lit;
                self.anim_tick = Some(self.anim_tick.unwrap_or(now) + BLINK);
            }
            Anim::Flash { left, lit } => {
                if *left == 0 {
                    self.anim = Anim::Steady;
                    self.anim_tick = None;
                } else {
                    *left -= 1;
                    *lit = !*lit;
                    self.anim_tick = Some(self.anim_tick.unwrap_or(now) + FLASH);
                }
            }
        }
    }
}

/// One OpenRGB device we drive, with the mode we picked for it.
struct Target {
    dev_idx: u32,
    name: String,
    led_count: u16,
    mode: Mode,
    /// The mode paints per-LED colors (`Direct`-style); otherwise the color
    /// rides along in the mode packet as a mode-specific color.
    per_led: bool,
    /// The mode packet has been sent at least once since (re)connecting.
    mode_pushed: bool,
}

struct Conn {
    write: OwnedWriteHalf,
    packets: mpsc::Receiver<Packet>,
    reader: JoinHandle<()>,
    targets: Vec<Target>,
    last: Option<Frame>,
}

/// Map a config brightness (0-100) into the mode's own range.
fn map_brightness(percent: u8, mode: &Mode) -> u32 {
    let (min, max) = (mode.brightness_min, mode.brightness_max);
    if max <= min {
        return min;
    }
    let span = u64::from(max - min);
    min + (span * u64::from(percent.min(100)) / 100) as u32
}

/// Prefer an explicit `Direct` mode, then any per-LED mode, then whatever is
/// active — the same order OpenRGB's own clients use to find a mode they can
/// drive frame by frame.
fn choose_mode(device: &Device) -> Option<&Mode> {
    device
        .modes
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("direct"))
        .or_else(|| device.modes.iter().find(|m| m.has_per_led_color()))
        .or_else(|| {
            usize::try_from(device.active_mode)
                .ok()
                .and_then(|i| device.modes.get(i))
        })
        .or_else(|| device.modes.first())
}

async fn reader_loop(mut read: OwnedReadHalf, tx: mpsc::Sender<Packet>) {
    loop {
        match proto::read_packet(&mut read).await {
            Ok(packet) => {
                if tx.send(packet).await.is_err() {
                    return;
                }
            }
            Err(err) => {
                debug!(err = format!("{err:#}"), "openrgb read ended");
                return;
            }
        }
    }
}

impl Conn {
    async fn connect(server: &str) -> Result<Self> {
        let stream = TcpStream::connect(server)
            .await
            .with_context(|| format!("cannot reach openrgb server at {server}"))?;
        stream.set_nodelay(true).ok();
        let (read, write) = stream.into_split();
        let (tx, packets) = mpsc::channel(16);
        let reader = tokio::spawn(reader_loop(read, tx));
        let mut conn = Self {
            write,
            packets,
            reader,
            targets: Vec::new(),
            last: None,
        };

        conn.send(&proto::req_client_name(CLIENT_NAME)).await?;
        conn.send(&proto::req_protocol_version()).await?;
        let reply = conn.await_packet(proto::pkt::PROTOCOL_VERSION).await?;
        let server_version = match reply.data.get(..4) {
            Some(bytes) => u32::from_le_bytes(bytes.try_into().unwrap()),
            None => bail!("server sent a malformed protocol version"),
        };
        if server_version < proto::PROTOCOL_VERSION {
            bail!(
                "openrgb speaks SDK protocol {server_version}, km-hub needs {} \
                 (upgrade OpenRGB)",
                proto::PROTOCOL_VERSION
            );
        }
        debug!(server_version, "openrgb handshake complete");

        conn.enumerate().await?;
        Ok(conn)
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.write.write_all(bytes).await.context("openrgb write")
    }

    /// Wait for a specific reply. Unsolicited packets (a device list update
    /// racing with enumeration) are ignored here — the next apply cycle
    /// re-reads the device list anyway.
    async fn await_packet(&mut self, id: u32) -> Result<Packet> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.packets.recv()).await {
                Ok(Some(packet)) if packet.id == id => return Ok(packet),
                Ok(Some(_)) => continue,
                Ok(None) => bail!("openrgb closed the connection"),
                Err(_) => bail!("openrgb did not answer packet {id} within {REPLY_TIMEOUT:?}"),
            }
        }
    }

    /// (Re)read the server's device list and pick a drivable mode per device.
    /// Called on connect and whenever the server announces a device change, so
    /// plugging the mouse in is picked up without restarting km-hub.
    async fn enumerate(&mut self) -> Result<()> {
        self.send(&proto::req_controller_count()).await?;
        let reply = self.await_packet(proto::pkt::CONTROLLER_COUNT).await?;
        let count = match reply.data.get(..4) {
            Some(bytes) => u32::from_le_bytes(bytes.try_into().unwrap()),
            None => bail!("server sent a malformed controller count"),
        };

        let mut targets = Vec::new();
        for index in 0..count {
            self.send(&proto::req_controller_data(index)).await?;
            let reply = self.await_packet(proto::pkt::CONTROLLER_DATA).await?;
            if reply.dev_idx != index {
                bail!(
                    "openrgb answered device {} for a request about {index}",
                    reply.dev_idx
                );
            }
            let device = proto::parse_device(index, &reply.data)
                .with_context(|| format!("device {index} description"))?;
            let Some(mode) = choose_mode(&device) else {
                debug!(device = %device.name, "no modes — skipping");
                continue;
            };
            let per_led = mode.has_per_led_color();
            if !per_led && !mode.has_mode_specific_color() {
                debug!(
                    device = %device.name,
                    mode = %mode.name,
                    "mode takes neither per-LED nor mode-specific colors — skipping"
                );
                continue;
            }
            debug!(
                device = %device.name,
                kind = device.kind,
                leds = device.led_count,
                mode = %mode.name,
                per_led,
                brightness = mode.has_brightness(),
                modes = device.modes.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(","),
                "driving device"
            );
            targets.push(Target {
                dev_idx: device.index,
                name: device.name.clone(),
                led_count: device.led_count,
                mode: mode.clone(),
                per_led,
                mode_pushed: false,
            });
        }

        if targets.is_empty() {
            info!("openrgb reports no drivable devices");
        }
        self.targets = targets;
        // Force the next apply to push everything to the new device set.
        self.last = None;
        Ok(())
    }

    async fn request_rescan(&mut self) -> Result<()> {
        debug!("asking openrgb to re-detect devices");
        self.send(&proto::req_rescan_devices()).await
    }

    async fn apply(&mut self, frame: Frame) -> Result<()> {
        if self.last == Some(frame) {
            return Ok(());
        }
        for i in 0..self.targets.len() {
            let mut packets: Vec<Vec<u8>> = Vec::with_capacity(2);
            {
                let target = &mut self.targets[i];
                let mut push_mode = !target.mode_pushed;

                // Brightness lives on the mode, and only where the mode says it
                // has one; devices without it (e.g. the G502) keep their own.
                if target.mode.has_brightness() {
                    let wanted = map_brightness(frame.brightness, &target.mode);
                    if target.mode.brightness != wanted {
                        target.mode.brightness = wanted;
                        push_mode = true;
                    }
                }

                if !target.per_led {
                    let wire = frame.color.to_wire();
                    if target.mode.colors.first() != Some(&wire)
                        || target.mode.color_mode != proto::MODE_COLORS_MODE_SPECIFIC
                    {
                        match target.mode.colors.first_mut() {
                            Some(slot) => *slot = wire,
                            None => target.mode.colors.push(wire),
                        }
                        target.mode.color_mode = proto::MODE_COLORS_MODE_SPECIFIC;
                        push_mode = true;
                    }
                }

                if push_mode {
                    packets.push(proto::update_mode(target.dev_idx, &target.mode));
                    target.mode_pushed = true;
                }
                if target.per_led {
                    packets.push(proto::update_leds(
                        target.dev_idx,
                        frame.color,
                        target.led_count,
                    ));
                }
            }
            for packet in &packets {
                let name = self.targets[i].name.clone();
                self.send(packet)
                    .await
                    .with_context(|| format!("updating '{name}'"))?;
            }
        }
        self.last = Some(frame);
        Ok(())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Schedule a rescan only when there is nothing to drive.
fn rescan_deadline(conn: &Conn) -> Option<Instant> {
    conn.targets
        .is_empty()
        .then(|| Instant::now() + RESCAN_INTERVAL)
}

/// `None` parks forever, so the select branch is inert while disconnected.
async fn next_packet(conn: &mut Option<Conn>) -> Option<Packet> {
    match conn {
        Some(conn) => conn.packets.recv().await,
        None => futures::future::pending().await,
    }
}

/// Same idea for Bluetooth events: once that channel is gone the branch parks
/// forever instead of ending the task.
async fn next_event(events: &mut Option<mpsc::Receiver<RgbEvent>>) -> Option<RgbEvent> {
    match events {
        Some(events) => events.recv().await,
        None => futures::future::pending().await,
    }
}

/// And for activity. A closed channel means the router is gone, so nothing can
/// keep the lighting awake any more — park instead of spinning on the error.
async fn next_activity(activity: &mut Option<watch::Receiver<Instant>>) -> Option<Instant> {
    match activity {
        Some(rx) => rx.changed().await.ok().map(|()| *rx.borrow_and_update()),
        None => futures::future::pending().await,
    }
}

pub async fn run(deps: RgbDeps) -> Result<()> {
    let RgbDeps {
        settings,
        mut slot_rx,
        activity_rx,
        event_rx,
        cancel,
    } = deps;
    // Taken away if the Bluetooth task ends: lighting is independent of it and
    // must keep showing the active slot either way.
    let mut event_rx = Some(event_rx);
    let mut activity_rx = Some(activity_rx);

    let server = settings.server.clone();
    let idle_timeout = settings.idle_timeout;
    let mut lights = Lights::new(settings, *slot_rx.borrow_and_update(), Instant::now());
    let mut conn: Option<Conn> = None;
    let mut backoff = Duration::from_secs(1);
    let mut retry_at = Instant::now();
    // Set only while the server knows of no devices (see RESCAN_INTERVAL).
    let mut rescan_at: Option<Instant> = None;
    // The first outage is worth a warning; the retries after it are not.
    let mut warned = false;

    match idle_timeout {
        Some(timeout) => info!(
            server = %server,
            idle_timeout_secs = timeout.as_secs(),
            "lighting enabled"
        ),
        None => info!(server = %server, "lighting enabled (never idles)"),
    }

    loop {
        if conn.is_none() && Instant::now() >= retry_at {
            match Conn::connect(&server).await {
                Ok(established) => {
                    info!(devices = established.targets.len(), "openrgb connected");
                    rescan_at = rescan_deadline(&established);
                    conn = Some(established);
                    backoff = Duration::from_secs(1);
                    warned = false;
                }
                Err(err) => {
                    let err = format!("{err:#}");
                    if warned {
                        debug!(%err, "openrgb still unavailable");
                    } else {
                        warn!(
                            %err,
                            "openrgb unavailable — no lighting until it returns (switching is unaffected)"
                        );
                        warned = true;
                    }
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }

        if let Some(established) = &mut conn
            && let Err(err) = established.apply(lights.frame()).await
        {
            debug!(err = format!("{err:#}"), "openrgb write failed — reconnecting");
            conn = None;
            rescan_at = None;
            retry_at = Instant::now() + backoff;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }

        let tick_at = lights.next_tick();
        let now = Instant::now();
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = slot_rx.changed() => lights.set_slot(*slot_rx.borrow_and_update()),
            at = next_activity(&mut activity_rx) => match at {
                Some(at) => lights.on_activity(at, Instant::now()),
                None => {
                    debug!("router gone — lighting stays as it is");
                    activity_rx = None;
                }
            },
            event = next_event(&mut event_rx) => match event {
                // Not an animation: this one reaches past `Lights` to the
                // connection, because the state that went wrong is the
                // hardware's, not ours.
                Some(RgbEvent::Repaint) => {
                    debug!("repaint requested — re-reading devices and pushing everything again");
                    if let Some(established) = &mut conn {
                        // `enumerate` re-reads every device and clears the
                        // last-applied frame, so the next apply repaints in
                        // full instead of deduplicating itself away.
                        if let Err(err) = established.enumerate().await {
                            debug!(err = format!("{err:#}"), "repaint failed — reconnecting");
                            conn = None;
                            rescan_at = None;
                            retry_at = Instant::now() + backoff;
                        } else {
                            rescan_at = rescan_deadline(established);
                        }
                    }
                }
                Some(event) => lights.on_event(event, Instant::now()),
                None => {
                    debug!("bluetooth events ended — slot lighting continues");
                    event_rx = None;
                }
            },
            _ = tokio::time::sleep_until(tick_at.unwrap_or(now)), if tick_at.is_some() => {
                lights.tick(Instant::now());
            }
            packet = next_packet(&mut conn) => match packet {
                Some(packet) if packet.id == proto::pkt::DEVICE_LIST_UPDATED => {
                    debug!("openrgb device list changed — re-enumerating");
                    if let Some(established) = &mut conn {
                        match established.enumerate().await {
                            Ok(()) => {
                                info!(devices = established.targets.len(), "openrgb device list updated");
                                rescan_at = rescan_deadline(established);
                            }
                            Err(err) => {
                                debug!(err = format!("{err:#}"), "re-enumeration failed — reconnecting");
                                conn = None;
                                rescan_at = None;
                                retry_at = Instant::now() + backoff;
                            }
                        }
                    }
                }
                Some(_) => {}
                None => {
                    debug!("openrgb connection closed");
                    conn = None;
                    rescan_at = None;
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            },
            _ = tokio::time::sleep_until(rescan_at.unwrap_or(now)), if rescan_at.is_some() => {
                // Nothing to drive: OpenRGB may have started before the USB
                // devices were ready. Ask it to look again, and keep asking.
                rescan_at = Some(Instant::now() + RESCAN_INTERVAL);
                if let Some(established) = &mut conn
                    && let Err(err) = established.request_rescan().await
                {
                    debug!(err = format!("{err:#}"), "rescan request failed — reconnecting");
                    conn = None;
                    rescan_at = None;
                    retry_at = Instant::now() + backoff;
                }
            }
            // Wake up when a reconnect is due.
            _ = tokio::time::sleep_until(retry_at), if conn.is_none() => {}
        }
    }

    // Deliberately no blank-out on exit: the last color stays as a hint that
    // the hub is idle.
    debug!("lighting stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{MODE_FLAG_HAS_BRIGHTNESS, MODE_FLAG_HAS_PER_LED_COLOR};

    fn mode(name: &str, flags: u32, min: u32, max: u32) -> Mode {
        Mode {
            index: 0,
            name: name.into(),
            value: 0,
            flags,
            speed_min: 0,
            speed_max: 0,
            brightness_min: min,
            brightness_max: max,
            colors_min: 0,
            colors_max: 1,
            speed: 0,
            brightness: 0,
            direction: 0,
            color_mode: 0,
            colors: Vec::new(),
        }
    }

    #[test]
    fn brightness_maps_into_the_modes_own_range() {
        let m = mode("Direct", MODE_FLAG_HAS_BRIGHTNESS, 0, 100);
        assert_eq!(map_brightness(0, &m), 0);
        assert_eq!(map_brightness(60, &m), 60);
        assert_eq!(map_brightness(100, &m), 100);

        let m = mode("Direct", MODE_FLAG_HAS_BRIGHTNESS, 10, 50);
        assert_eq!(map_brightness(0, &m), 10);
        assert_eq!(map_brightness(50, &m), 30);
        assert_eq!(map_brightness(100, &m), 50);
        // Over-range input is clamped, not wrapped.
        assert_eq!(map_brightness(255, &m), 50);

        // A mode with no usable range collapses to its minimum.
        assert_eq!(map_brightness(75, &mode("Static", 0, 7, 7)), 7);
    }

    #[test]
    fn devices_without_the_brightness_flag_are_left_alone() {
        let m = mode("Direct", MODE_FLAG_HAS_PER_LED_COLOR, 0, 100);
        assert!(!m.has_brightness());
        assert!(m.has_per_led_color());
    }

    fn device(modes: Vec<Mode>, active: i32) -> Device {
        Device {
            index: 0,
            name: "test".into(),
            kind: 0,
            active_mode: active,
            modes,
            led_count: 4,
        }
    }

    #[test]
    fn mode_choice_prefers_direct_then_per_led_then_active() {
        let dev = device(
            vec![
                mode("Static", 0, 0, 0),
                mode("Rainbow", MODE_FLAG_HAS_PER_LED_COLOR, 0, 0),
                mode("Direct", MODE_FLAG_HAS_PER_LED_COLOR, 0, 0),
            ],
            0,
        );
        assert_eq!(choose_mode(&dev).unwrap().name, "Direct");

        let dev = device(
            vec![
                mode("Static", 0, 0, 0),
                mode("Rainbow", MODE_FLAG_HAS_PER_LED_COLOR, 0, 0),
            ],
            0,
        );
        assert_eq!(choose_mode(&dev).unwrap().name, "Rainbow");

        let dev = device(vec![mode("Static", 0, 0, 0), mode("Breathing", 0, 0, 0)], 1);
        assert_eq!(choose_mode(&dev).unwrap().name, "Breathing");

        assert!(choose_mode(&device(Vec::new(), 0)).is_none());
    }

    /// Animation tests: idle blanking off, so every deadline and every frame
    /// belongs to the animation under test.
    fn lights() -> Lights {
        idle_lights(None).0
    }

    /// Idle tests: an explicit timeout, plus the instant the clock starts from.
    fn idle_lights(idle_timeout: Option<Duration>) -> (Lights, Instant) {
        let mut settings = RgbSettings::for_test();
        settings.idle_timeout = idle_timeout;
        let start = Instant::now();
        (Lights::new(settings, 1, start), start)
    }

    /// Jump to the next scheduled deadline and fire it, exactly as `run`'s
    /// timer arm does. Returns the instant it fired at.
    fn advance(l: &mut Lights) -> Instant {
        let at = l.next_tick().expect("nothing scheduled");
        l.tick(at);
        at
    }

    /// Run the fade to a standstill. Bounded so a state machine that never
    /// settles fails the test instead of hanging it.
    fn settle_fade(l: &mut Lights) {
        for _ in 0..64 {
            match l.fade_tick {
                Some(at) => l.tick(at),
                None => return,
            }
        }
        panic!("fade never settled (level {})", l.level);
    }

    #[test]
    fn pairing_blinks_and_bind_flashes_twice_before_settling() {
        let mut l = lights();
        let steady = l.frame();

        l.on_event(RgbEvent::PairingOpen, Instant::now());
        assert_eq!(l.frame().color, l.settings.pairing_color);
        advance(&mut l);
        assert_eq!(l.frame().color, Rgb::OFF);
        advance(&mut l);
        assert_eq!(l.frame().color, l.settings.pairing_color);
        // Blinking holds the slot's brightness so only colors change.
        assert_eq!(l.frame().brightness, steady.brightness);

        l.on_event(RgbEvent::Bound, Instant::now());
        let mut lit = Vec::new();
        for _ in 0..4 {
            lit.push(l.frame().color != Rgb::OFF);
            advance(&mut l);
        }
        assert_eq!(lit, vec![true, false, true, false]);
        assert_eq!(l.frame(), steady);
        assert!(l.next_tick().is_none());
    }

    #[test]
    fn pairing_timeout_returns_to_the_slot_color() {
        let mut l = lights();
        let steady = l.frame();
        l.on_event(RgbEvent::PairingOpen, Instant::now());
        assert_ne!(l.frame(), steady);
        l.on_event(RgbEvent::PairingClosed, Instant::now());
        assert_eq!(l.frame(), steady);
        assert!(l.next_tick().is_none());
    }

    #[test]
    fn the_desk_going_quiet_fades_the_lighting_out_and_input_brings_it_back() {
        let timeout = Duration::from_secs(120);
        let (mut l, start) = idle_lights(Some(timeout));
        let steady = l.frame();
        assert_ne!(steady.color, Rgb::OFF, "the test slot must start lit");

        // A second short of the deadline nothing has moved.
        l.tick(start + timeout - Duration::from_secs(1));
        assert_eq!(l.frame(), steady);

        // Past it, the fade runs all the way to black and then stops
        // scheduling anything at all.
        let at = start + timeout;
        l.tick(at);
        settle_fade(&mut l);
        assert_eq!(l.level, 0);
        assert_eq!(l.frame().color, Rgb::OFF);
        assert!(l.next_tick().is_none(), "a dark, idle hub should be asleep");
        // Brightness is untouched: the fade rides on the color so that devices
        // with no brightness control fade too.
        assert_eq!(l.frame().brightness, steady.brightness);

        // A keystroke brings it back, exactly as it was.
        l.on_activity(at, at);
        settle_fade(&mut l);
        assert_eq!(l.frame(), steady);
        assert_eq!(l.next_tick(), Some(at + timeout), "the countdown restarts");
    }

    #[test]
    fn an_interrupted_fade_out_reverses_from_where_it_is() {
        let timeout = Duration::from_secs(120);
        let (mut l, start) = idle_lights(Some(timeout));

        // Caught well into the fade — far enough down that a single step back
        // up cannot reach full, so the assertions below mean something.
        const CAUGHT_AFTER: u8 = 10;
        let mut at = start + timeout;
        l.tick(at);
        for _ in 0..CAUGHT_AFTER {
            at = l.fade_tick.expect("fading out");
            l.tick(at);
        }
        let caught = l.level;
        assert_eq!(caught, u8::MAX - CAUGHT_AFTER * FADE_OUT_STEP);
        assert!(caught + FADE_IN_STEP < u8::MAX, "caught too early to tell");

        // Interrupting it neither jumps to full nor restarts from black.
        l.on_activity(at, at);
        assert_eq!(l.level, caught, "the level moved on its own");
        at = l.fade_tick.expect("fading back in");
        l.tick(at);
        assert_eq!(l.level, caught + FADE_IN_STEP);

        // And it still gets all the way home.
        settle_fade(&mut l);
        assert_eq!(l.level, u8::MAX);
    }

    #[test]
    fn the_idle_gate_masks_animations_without_losing_them() {
        let timeout = Duration::from_secs(120);
        let (mut l, start) = idle_lights(Some(timeout));

        l.on_event(RgbEvent::PairingOpen, start);
        let at = start + timeout;
        l.tick(at);
        settle_fade(&mut l);

        // Dark wins over the pairing blink...
        assert_eq!(l.level, 0);
        assert_eq!(l.frame().color, Rgb::OFF);
        // ...but the blink is still running behind it, so it is showing the
        // right phase the moment the user comes back.
        assert!(matches!(l.anim, Anim::Pairing { .. }), "the animation was lost");
        assert!(l.anim_tick.is_some(), "the animation stopped ticking");

        l.on_activity(at, at);
        settle_fade(&mut l);
        let Anim::Pairing { lit } = l.anim else {
            unreachable!("checked above");
        };
        assert_eq!(l.frame(), l.blink(lit, l.settings.pairing_color));
    }

    /// `idle_timeout_secs = 0` parses to `None` — the documented "never blank".
    #[test]
    fn blanking_can_be_switched_off_entirely() {
        let (mut l, start) = idle_lights(None);
        let steady = l.frame();
        assert!(l.next_tick().is_none(), "no deadline should be armed");

        l.tick(start + Duration::from_secs(86_400));
        assert_eq!(l.frame(), steady);
        assert!(l.next_tick().is_none());
    }

    #[test]
    fn activity_pings_are_coalesced_but_never_delay_a_wake() {
        let (tx, mut rx) = watch::channel(Instant::now());
        let mut activity = Activity::new(tx);
        rx.borrow_and_update();
        let start = Instant::now();

        // The first event is always sent — this is the keystroke that wakes a
        // dark desk, and it must not wait for a coalescing window.
        activity.ping(start);
        assert!(rx.has_changed().unwrap(), "the first ping was swallowed");
        assert_eq!(*rx.borrow_and_update(), start);

        // A burst inside the window collapses to nothing further.
        for ms in 1..1000 {
            activity.ping(start + Duration::from_millis(ms));
        }
        assert!(!rx.has_changed().unwrap(), "a typing burst was not coalesced");

        // Once the window is up, the next event goes out immediately again.
        let later = start + ACTIVITY_COALESCE;
        activity.ping(later);
        assert!(rx.has_changed().unwrap(), "ping dropped after the window");
        assert_eq!(*rx.borrow_and_update(), later);
    }

    /// Minimal OpenRGB server: answers the handshake, reports no devices.
    /// Returns once a client has completed enumeration.
    async fn fake_server(listener: tokio::net::TcpListener) -> Result<()> {
        let (stream, _) = listener.accept().await?;
        let (mut read, mut write) = stream.into_split();
        loop {
            let packet = proto::read_packet(&mut read).await?;
            match packet.id {
                proto::pkt::PROTOCOL_VERSION => {
                    write.write_all(&proto_reply(packet.id, &5u32.to_le_bytes())).await?
                }
                proto::pkt::CONTROLLER_COUNT => {
                    write.write_all(&proto_reply(packet.id, &0u32.to_le_bytes())).await?;
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    /// Like `fake_server`, but stays up and reports every enumeration it is
    /// asked for, so a test can tell a repaint actually reached the wire.
    async fn counting_server(
        listener: tokio::net::TcpListener,
        enumerations: mpsc::Sender<()>,
    ) -> Result<()> {
        let (stream, _) = listener.accept().await?;
        let (mut read, mut write) = stream.into_split();
        loop {
            let packet = proto::read_packet(&mut read).await?;
            match packet.id {
                proto::pkt::PROTOCOL_VERSION => {
                    write.write_all(&proto_reply(packet.id, &5u32.to_le_bytes())).await?
                }
                proto::pkt::CONTROLLER_COUNT => {
                    write.write_all(&proto_reply(packet.id, &0u32.to_le_bytes())).await?;
                    if enumerations.send(()).await.is_err() {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }

    /// The user's retry for lighting that came out wrong: pressing the combo
    /// for the slot already active must go back to the hardware, not be
    /// deduplicated away as "nothing changed".
    #[tokio::test]
    async fn a_repaint_re_reads_the_device_list() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (enum_tx, mut enumerations) = mpsc::channel(8);
        tokio::spawn(counting_server(listener, enum_tx));

        let mut settings = RgbSettings::for_test();
        settings.server = addr.to_string();
        let (_slot_tx, slot_rx) = watch::channel(1u8);
        let (_activity_tx, activity_rx) = watch::channel(Instant::now());
        let (event_tx, event_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(RgbDeps {
            settings,
            slot_rx,
            activity_rx,
            event_rx,
            cancel: cancel.clone(),
        }));

        let wait = Duration::from_secs(10);
        tokio::time::timeout(wait, enumerations.recv())
            .await
            .expect("never enumerated on connect")
            .expect("server gone");

        event_tx.send(RgbEvent::Repaint).await.unwrap();
        tokio::time::timeout(wait, enumerations.recv())
            .await
            .expect("a repaint did not re-read the device list")
            .expect("server gone");

        cancel.cancel();
        let _ = task.await;
    }

    fn proto_reply(id: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"ORGB");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// The hub boots faster than OpenRGB does: km-hub's first connect is
    /// refused, and it must keep retrying until the server appears.
    #[tokio::test]
    async fn reconnects_after_the_server_shows_up_late() {
        // Reserve a port, then free it so the first connect is refused.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let mut settings = RgbSettings::for_test();
        settings.server = addr.to_string();
        let (_slot_tx, slot_rx) = watch::channel(1u8);
        let (_activity_tx, activity_rx) = watch::channel(Instant::now());
        let (event_tx, event_rx) = mpsc::channel(4);
        // The Bluetooth task owns the event sender. If it dies (bluetoothd not
        // ready yet at boot) the channel closes — lighting must carry on.
        drop(event_tx);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(RgbDeps {
            settings,
            slot_rx,
            activity_rx,
            event_rx,
            cancel: cancel.clone(),
        }));

        // Come up well after the first attempt was refused.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let served = tokio::time::timeout(Duration::from_secs(10), fake_server(listener)).await;

        cancel.cancel();
        let _ = task.await;
        served
            .expect("km-hub never reconnected after the server appeared")
            .expect("handshake failed");
    }

    #[test]
    fn switching_slots_mid_blink_shows_the_new_color_once_it_ends() {
        let mut l = lights();
        l.on_event(RgbEvent::PairingOpen, Instant::now());
        l.set_slot(3);
        assert_eq!(l.frame().color, l.settings.pairing_color);
        l.on_event(RgbEvent::PairingClosed, Instant::now());
        assert_eq!(l.frame().color, l.settings.slot_color(3));
    }
}
