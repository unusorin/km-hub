//! Input device discovery, grabbing, async readers and the uinput passthrough sink.

pub mod hotkey;
pub mod keys;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, Device, EventType, InputEvent, KeyCode, RelativeAxisCode};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::router::RouterMsg;

/// Name prefix for our own uinput devices; discovery must skip these or we
/// would read back the events we inject (feedback loop).
pub const VIRTUAL_PREFIX: &str = "km-hub virtual";

/// Discover the devices to manage: explicit paths from the config, or
/// auto-detection of everything that looks like a keyboard, mouse or media
/// collection.
pub fn discover(explicit: &Option<Vec<PathBuf>>) -> Result<Vec<(PathBuf, Device)>> {
    let mut found = Vec::new();
    match explicit {
        Some(paths) => {
            for path in paths {
                let dev = Device::open(path)
                    .with_context(|| format!("cannot open input device {}", path.display()))?;
                found.push((path.clone(), dev));
            }
        }
        None => {
            for (path, dev) in evdev::enumerate() {
                let name = dev.name().unwrap_or("");
                if name.starts_with(VIRTUAL_PREFIX) {
                    continue;
                }
                if is_keyboard(&dev) || is_mouse(&dev) || is_consumer(&dev) {
                    info!(device = %path.display(), name, "managing input device");
                    found.push((path, dev));
                } else {
                    debug!(device = %path.display(), name, "skipping (not keyboard/mouse)");
                }
            }
        }
    }
    if found.is_empty() {
        bail!(
            "no input devices found — check permissions on /dev/input/event* \
             (run ./setup.sh, then log out and back in)"
        );
    }
    Ok(found)
}

pub fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .map(|keys| keys.contains(KeyCode::KEY_A))
        .unwrap_or(false)
}

/// Media collections. A keyboard's volume roller and transport keys usually sit
/// on a node of their own that has no KEY_A and no pointer — the Apex 3's is
/// KEY_ESC, KEY_ENTER and the media keys — so neither predicate above claims it
/// and the keys would never be grabbed at all.
///
/// Restricted to attached peripherals: HDMI CEC receivers advertise the very
/// same media keys, and grabbing those takes the monitor's remote away from the
/// host and turns anything the display emits — plugging in, or the input switch
/// the slot hooks drive — into keystrokes for whichever target is active.
fn is_consumer(dev: &Device) -> bool {
    if !matches!(
        dev.input_id().bus_type(),
        BusType::BUS_USB | BusType::BUS_BLUETOOTH
    ) {
        return false;
    }
    dev.supported_keys()
        .map(|keys| {
            keys.contains(KeyCode::KEY_VOLUMEUP)
                || keys.contains(KeyCode::KEY_MUTE)
                || keys.contains(KeyCode::KEY_PLAYPAUSE)
        })
        .unwrap_or(false)
}

fn is_mouse(dev: &Device) -> bool {
    let has_rel = dev
        .supported_relative_axes()
        .map(|axes| axes.contains(RelativeAxisCode::REL_X))
        .unwrap_or(false);
    let has_btn = dev
        .supported_keys()
        .map(|keys| keys.contains(KeyCode::BTN_LEFT))
        .unwrap_or(false);
    has_rel && has_btn
}

/// Grab each device and spawn one reader task per device that forwards raw
/// events to the router. Dropping the `Device` on task exit ungrabs it.
pub fn spawn_readers(
    devices: Vec<(PathBuf, Device)>,
    tx: mpsc::Sender<RouterMsg>,
    cancel: CancellationToken,
) -> Result<Vec<JoinHandle<()>>> {
    let mut handles = Vec::new();
    for (path, mut dev) in devices {
        dev.grab()
            .with_context(|| format!("cannot grab {}", path.display()))?;
        let mut stream = dev
            .into_event_stream()
            .with_context(|| format!("cannot stream {}", path.display()))?;
        let tx = tx.clone();
        let cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    ev = stream.next_event() => match ev {
                        Ok(event) => {
                            if tx.send(RouterMsg::Input(event)).await.is_err() {
                                break; // router gone
                            }
                        }
                        Err(err) => {
                            warn!(device = %path.display(), %err, "reader stopped (device unplugged?)");
                            break;
                        }
                    },
                }
            }
        }));
    }
    Ok(handles)
}

/// Local passthrough sink: virtual keyboard + mouse that re-inject events into
/// the local session while the local target is active.
pub struct LocalSink {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
}

impl LocalSink {
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        // All standard keyboard keys (KEY_ESC .. KEY_MICMUTE).
        for code in 1..=248u16 {
            keys.insert(KeyCode::new(code));
        }
        let keyboard = VirtualDevice::builder()
            .context("cannot open /dev/uinput — run ./setup.sh, then log out and back in")?
            .name(&format!("{VIRTUAL_PREFIX} keyboard"))
            .with_keys(&keys)?
            .build()?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        for code in KeyCode::BTN_LEFT.code()..=KeyCode::BTN_TASK.code() {
            buttons.insert(KeyCode::new(code));
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        axes.insert(RelativeAxisCode::REL_WHEEL);
        axes.insert(RelativeAxisCode::REL_HWHEEL);
        let mouse = VirtualDevice::builder()?
            .name(&format!("{VIRTUAL_PREFIX} mouse"))
            .with_keys(&buttons)?
            .with_relative_axes(&axes)?
            .build()?;

        Ok(Self { keyboard, mouse })
    }

    /// Re-inject one event into the local session. SYN events are skipped —
    /// `VirtualDevice::emit` appends its own SYN_REPORT.
    pub fn emit(&mut self, event: &InputEvent) -> Result<()> {
        match event.event_type() {
            EventType::KEY => {
                let is_button = (KeyCode::BTN_LEFT.code()..=KeyCode::BTN_TASK.code())
                    .contains(&event.code());
                if is_button {
                    self.mouse.emit(&[*event])?;
                } else {
                    self.keyboard.emit(&[*event])?;
                }
            }
            EventType::RELATIVE => self.mouse.emit(&[*event])?,
            _ => {}
        }
        Ok(())
    }

    /// Synthesize release events locally for the given keys (used when
    /// switching away from the local target so nothing stays stuck).
    pub fn release_keys(&mut self, keys: &[KeyCode]) -> Result<()> {
        for key in keys {
            let ev = InputEvent::new(EventType::KEY.0, key.code(), 0);
            self.emit(&ev)?;
        }
        Ok(())
    }
}
