//! HID over GATT (HOGP): the hub as a Bluetooth LE peripheral.
//!
//! This is the transport Apple's own keyboards and mice use, and the reason
//! it exists here: over classic HID the hub is a *slave* that a macOS/iOS
//! host polls at its leisure — every 34–52 ms in the captures — while an LE
//! connection interval (11.25–15 ms for HID) is a reservation the host keeps.
//!
//! The GATT database is built with bluer's local GATT server; every input
//! report characteristic uses the `Io` notify method, which hands us one
//! `CharacteristicWriter` per subscribing host (BlueZ calls `AcquireNotify`
//! for each), so reports can be routed per peer exactly like the L2CAP
//! transport routes by address. Hosts connect to *us* whenever we advertise;
//! there is no dialing on LE.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bluer::adv::Advertisement;
use bluer::gatt::CharacteristicWriter;
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicControl, CharacteristicControlEvent,
    CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicRead, CharacteristicWrite,
    CharacteristicWriteMethod, Descriptor, DescriptorRead, ReqError, ReqResult, Service,
    characteristic_control,
};
use bluer::{Adapter, Address, Uuid, UuidExt};
use futures::stream::{SelectAll, Stream, StreamExt, select_all};
use futures::FutureExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::hid::{BATTERY_LEVEL, BATTERY_REPORT_ID, HidFrame, REPORT_DESCRIPTOR};

// Assigned numbers (Bluetooth SIG).
const SVC_HID: u16 = 0x1812;
const SVC_BATTERY: u16 = 0x180F;
const SVC_DEVICE_INFO: u16 = 0x180A;
const CHR_HID_INFORMATION: u16 = 0x2A4A;
const CHR_REPORT_MAP: u16 = 0x2A4B;
const CHR_HID_CONTROL_POINT: u16 = 0x2A4C;
const CHR_REPORT: u16 = 0x2A4D;
const CHR_PROTOCOL_MODE: u16 = 0x2A4E;
const CHR_BATTERY_LEVEL: u16 = 0x2A19;
const CHR_MANUFACTURER_NAME: u16 = 0x2A29;
const CHR_PNP_ID: u16 = 0x2A50;
const DSC_REPORT_REFERENCE: u16 = 0x2908;

/// GAP appearance: HID keyboard. The report map also carries the mouse; a
/// combo advertises as its keyboard half, matching the classic class of
/// device (keyboard/pointing combo).
pub const APPEARANCE_HID_KEYBOARD: u16 = 0x03C1;

/// Report types in a Report Reference descriptor.
const REPORT_TYPE_INPUT: u8 = 1;
const REPORT_TYPE_FEATURE: u8 = 3;

/// Report IDs from `hid::REPORT_DESCRIPTOR`.
pub const REPORT_ID_KEYBOARD: u8 = 1;
pub const REPORT_ID_MOUSE: u8 = 2;
pub const REPORT_ID_CONSUMER: u8 = 4;
/// Input report IDs and their zero-state payload lengths (ID byte excluded).
const INPUT_REPORTS: [(u8, usize); 3] = [(REPORT_ID_KEYBOARD, 8), (REPORT_ID_MOUSE, 4), (REPORT_ID_CONSUMER, 2)];

/// PnP ID: vendor ID source USB, VID Linux Foundation, product 1, version 1.0.
/// HOGP requires a PnP ID; not Apple's VID.
const PNP_VENDOR_SOURCE_USB: u8 = 0x02;
const PNP_VID: u16 = 0x1D6B;
const PNP_PID: u16 = 0x0001;
const PNP_VERSION: u16 = 0x0100;

/// How long a key/button report may wait for send space before the
/// subscription is declared dead. Short on purpose: while `send_to` blocks,
/// the Manager loop is away from `accept_incoming`, and BlueZ's `AcquireNotify`
/// reply for another host waits on that.
const KEY_SEND_TIMEOUT: Duration = Duration::from_millis(250);

/// HID Information: bcdHID 1.11, country code 0, flags RemoteWake |
/// NormallyConnectable.
pub fn hid_information_value() -> Vec<u8> {
    vec![0x11, 0x01, 0x00, 0x03]
}

pub fn pnp_id_value() -> Vec<u8> {
    let mut v = vec![PNP_VENDOR_SOURCE_USB];
    v.extend_from_slice(&PNP_VID.to_le_bytes());
    v.extend_from_slice(&PNP_PID.to_le_bytes());
    v.extend_from_slice(&PNP_VERSION.to_le_bytes());
    v
}

/// Report Reference descriptor value: `[report id, report type]`.
pub fn report_reference(id: u8, report_type: u8) -> Vec<u8> {
    vec![id, report_type]
}

/// Serve a long attribute value: hosts read past the first MTU with Read Blob
/// requests carrying an offset, and BlueZ hands that offset to us. The 142-byte
/// report map needs this at the default 23-byte MTU.
pub fn read_at_offset(value: &[u8], offset: u16) -> ReqResult<Vec<u8>> {
    let offset = usize::from(offset);
    if offset > value.len() {
        return Err(ReqError::InvalidOffset);
    }
    Ok(value[offset..].to_vec())
}

/// A mouse report that only moves the pointer (buttons unchanged since the
/// last one sent) may be dropped under back-pressure: the next one carries
/// the newer position anyway. Anything else is state the host must see.
pub fn is_motion_only(frame: &HidFrame, last_mouse_buttons: u8) -> bool {
    match frame {
        HidFrame::Mouse(b) => b[1] == last_mouse_buttons,
        HidFrame::Keyboard(_) | HidFrame::Consumer(_) => false,
    }
}

/// The LE advertisement: connectable HID peripheral, discoverable only while a
/// pairing window is open. Bonded hosts reconnect to a non-discoverable
/// advertisement on their own; strangers can't see it.
pub fn advertisement(alias: &str, discoverable: bool) -> Advertisement {
    Advertisement {
        service_uuids: [Uuid::from_u16(SVC_HID)].into_iter().collect(),
        appearance: Some(APPEARANCE_HID_KEYBOARD),
        local_name: Some(alias.to_string()),
        discoverable: Some(discoverable),
        ..Default::default()
    }
}

fn read_only(encrypt: bool, value: impl Fn() -> ReqResult<Vec<u8>> + Send + Sync + 'static, what: &'static str) -> CharacteristicRead {
    CharacteristicRead {
        read: true,
        encrypt_read: encrypt,
        fun: Box::new(move |req| {
            debug!(device = %req.device_address, link = ?req.link, offset = req.offset, what, "GATT read");
            let res = value().and_then(|v| read_at_offset(&v, req.offset));
            async move { res }.boxed()
        }),
        ..Default::default()
    }
}

fn report_reference_descriptor(id: u8, report_type: u8) -> Descriptor {
    Descriptor {
        uuid: Uuid::from_u16(DSC_REPORT_REFERENCE),
        read: Some(DescriptorRead {
            read: true,
            encrypt_read: true,
            fun: Box::new(move |_req| async move { Ok(report_reference(id, report_type)) }.boxed()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// One input report characteristic: readable zero state, notify via `Io` so
/// every subscribing host gets its own writer.
fn input_report(id: u8, len: usize) -> (Characteristic, CharacteristicControl) {
    let (control, control_handle) = characteristic_control();
    let chr = Characteristic {
        uuid: Uuid::from_u16(CHR_REPORT),
        read: Some(read_only(true, move || Ok(vec![0u8; len]), "input report")),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Io,
            ..Default::default()
        }),
        descriptors: vec![report_reference_descriptor(id, REPORT_TYPE_INPUT)],
        control_handle,
        ..Default::default()
    };
    (chr, control)
}

fn hid_service() -> (Service, Vec<(u8, CharacteristicControl)>) {
    let mut controls = Vec::new();
    let mut characteristics = vec![
        Characteristic {
            uuid: Uuid::from_u16(CHR_HID_INFORMATION),
            read: Some(read_only(true, || Ok(hid_information_value()), "HID information")),
            ..Default::default()
        },
        Characteristic {
            uuid: Uuid::from_u16(CHR_REPORT_MAP),
            read: Some(read_only(true, || Ok(REPORT_DESCRIPTOR.to_vec()), "report map")),
            ..Default::default()
        },
        Characteristic {
            uuid: Uuid::from_u16(CHR_HID_CONTROL_POINT),
            write: Some(CharacteristicWrite {
                write_without_response: true,
                encrypt_write: true,
                method: CharacteristicWriteMethod::Fun(Box::new(|value, req| {
                    let what = match value.first() {
                        Some(0) => "suspend",
                        Some(1) => "exit suspend",
                        _ => "unknown",
                    };
                    info!(device = %req.device_address, what, "HID control point written");
                    async move { Ok::<(), ReqError>(()) }.boxed()
                })),
                ..Default::default()
            }),
            ..Default::default()
        },
        Characteristic {
            uuid: Uuid::from_u16(CHR_PROTOCOL_MODE),
            // 1 = report protocol. Boot protocol is acknowledged but not
            // honored, the same stance as the L2CAP control channel.
            read: Some(read_only(true, || Ok(vec![1]), "protocol mode")),
            write: Some(CharacteristicWrite {
                write_without_response: true,
                encrypt_write: true,
                method: CharacteristicWriteMethod::Fun(Box::new(|value, req| {
                    if value.first() == Some(&0) {
                        info!(device = %req.device_address, "host requested boot protocol — acknowledged but streaming report protocol");
                    } else {
                        debug!(device = %req.device_address, ?value, "protocol mode written");
                    }
                    async move { Ok::<(), ReqError>(()) }.boxed()
                })),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    for (id, len) in INPUT_REPORTS {
        let (chr, control) = input_report(id, len);
        characteristics.push(chr);
        controls.push((id, control));
    }
    // The battery feature report the map declares (ID 3). Hosts enumerate
    // Report characteristics against the map; keep the two consistent.
    characteristics.push(Characteristic {
        uuid: Uuid::from_u16(CHR_REPORT),
        read: Some(read_only(true, || Ok(vec![BATTERY_LEVEL]), "battery feature report")),
        write: Some(CharacteristicWrite {
            write: true,
            encrypt_write: true,
            method: CharacteristicWriteMethod::Fun(Box::new(|value, req| {
                debug!(device = %req.device_address, ?value, "battery feature report written (ignored)");
                async move { Ok::<(), ReqError>(()) }.boxed()
            })),
            ..Default::default()
        }),
        descriptors: vec![report_reference_descriptor(BATTERY_REPORT_ID, REPORT_TYPE_FEATURE)],
        ..Default::default()
    });
    let service = Service {
        uuid: Uuid::from_u16(SVC_HID),
        primary: true,
        characteristics,
        ..Default::default()
    };
    (service, controls)
}

fn battery_service() -> Service {
    Service {
        uuid: Uuid::from_u16(SVC_BATTERY),
        primary: true,
        characteristics: vec![Characteristic {
            uuid: Uuid::from_u16(CHR_BATTERY_LEVEL),
            read: Some(read_only(false, || Ok(vec![BATTERY_LEVEL]), "battery level")),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn device_information_service() -> Service {
    Service {
        uuid: Uuid::from_u16(SVC_DEVICE_INFO),
        primary: true,
        characteristics: vec![
            Characteristic {
                uuid: Uuid::from_u16(CHR_MANUFACTURER_NAME),
                read: Some(read_only(false, || Ok(b"km-hub".to_vec()), "manufacturer name")),
                ..Default::default()
            },
            Characteristic {
                uuid: Uuid::from_u16(CHR_PNP_ID),
                read: Some(read_only(false, || Ok(pnp_id_value()), "PnP ID")),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

/// The complete GATT application plus the notify control stream of each input
/// report characteristic, tagged with its report ID.
pub fn build_hid_application() -> (Application, Vec<(u8, CharacteristicControl)>) {
    let (hid, controls) = hid_service();
    let app = Application {
        services: vec![hid, battery_service(), device_information_service()],
        ..Default::default()
    };
    (app, controls)
}

type TaggedEvents = SelectAll<Pin<Box<dyn Stream<Item = (u8, CharacteristicControlEvent)> + Send>>>;

/// One host's subscription to one input report.
struct Sub {
    writer: Arc<CharacteristicWriter>,
    generation: u64,
    /// Resolves `writer.closed()` into a `closed_tx` message; aborted on drop
    /// (the same pattern as `transport::Conn` and its control task).
    watcher: JoinHandle<()>,
}

impl Drop for Sub {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

#[derive(Default)]
struct Peer {
    /// report id → subscription; a host that has not enabled a report's
    /// notifications has no entry for it.
    subs: HashMap<u8, Sub>,
    /// Buttons byte of the last mouse report sent, for `is_motion_only`.
    last_mouse_buttons: u8,
}

/// Register the HID application and tag every characteristic's control
/// stream with its report id.
async fn register(adapter: &Adapter) -> Result<(ApplicationHandle, TaggedEvents)> {
    let (app, controls) = build_hid_application();
    let handle = adapter
        .serve_gatt_application(app)
        .await
        .context("HID GATT application registration failed")?;
    info!("HID GATT application registered (HID, Battery, Device Information services)");
    let events = select_all(
        controls
            .into_iter()
            .map(|(id, control)| control.map(move |ev| (id, ev)).boxed()),
    );
    Ok((handle, events))
}

/// Poll the adapter's local service list until the HID service is (`present`
/// = true) or is not there; false on timeout. bluer unregisters an application
/// from a spawned task, so a dropped handle is not yet gone.
async fn wait_hid_service(adapter: &Adapter, present: bool) -> bool {
    let hid = Uuid::from_u16(SVC_HID);
    let deadline = tokio::time::Instant::now() + REREGISTER_WAIT;
    loop {
        let has = adapter
            .uuids()
            .await
            .ok()
            .flatten()
            .is_some_and(|set| set.contains(&hid));
        if has == present {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Upper bound on waiting for bluetoothd to drop the old application.
const REREGISTER_WAIT: Duration = Duration::from_secs(2);

/// How long `LeTransport::shutdown` waits for one host to drop its link.
/// bluetoothd's Device1.Disconnect first lets the profiles disconnect and only
/// forces the ACL down after a fixed 2 s timer (device.c DISCONNECT_TIMER), so
/// the reply never comes sooner than that.
const SHUTDOWN_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Holds one GATT application and one set of writers per subscribed host, so
/// frames are routed by peer address like the L2CAP transport.
pub struct LeTransport {
    adapter: Adapter,
    app: Option<ApplicationHandle>,
    events: TaggedEvents,
    /// False once `events` ends (application unregistered) so the Manager
    /// stops selecting on `accept_incoming`.
    listening: bool,
    closed_tx: mpsc::UnboundedSender<(Address, u8, u64)>,
    closed_rx: mpsc::UnboundedReceiver<(Address, u8, u64)>,
    next_generation: u64,
    peers: HashMap<Address, Peer>,
}

impl LeTransport {
    /// Register the HID GATT application on `adapter`.
    pub async fn serve(adapter: Adapter) -> Result<Self> {
        let (app, events) = register(&adapter).await?;
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        Ok(Self {
            adapter,
            app: Some(app),
            events,
            listening: true,
            closed_tx,
            closed_rx,
            next_generation: 0,
            peers: HashMap::new(),
        })
    }

    /// Replace the GATT application with a fresh copy of itself.
    ///
    /// bluetoothd remembers a *bonded* host's CCC ("notifications on") across
    /// disconnects and answers the host's re-enable on reconnect with "no
    /// change" — without ever calling AcquireNotify again (gatt-database.c:
    /// `if (val == ccc->value) goto done;`, and `att_disconnected` keeps the
    /// state for bonded devices). The host then sits connected, HID attached,
    /// and hears nothing from us. The one thing that clears that state short
    /// of restarting bluetoothd is removing the service (`remove_device_ccc`).
    ///
    /// Order matters: unregister the old application, wait until its services
    /// are really gone, and only then register the new one. A BlueZ host
    /// (device.c) skips probing a service whose UUID it already has a profile
    /// for, and on removal keeps the profile as long as *another* service
    /// with that UUID exists — so "add new, then drop old" leaves its HID
    /// profile bound to dead handles for good. Remove-then-add makes it drop
    /// HoG with the last HID service and re-probe it on the new one; that
    /// costs every connected host two Service Changed round trips and a
    /// moment without HID, which is why the caller does this sparingly.
    pub async fn reregister(&mut self) -> Result<()> {
        // Old sessions die with the old attributes; the watchers report them
        // and `forget_sub` clears the peers.
        self.app.take();
        self.events = SelectAll::new();
        if !wait_hid_service(&self.adapter, false).await {
            warn!("old HID application still registered after {REREGISTER_WAIT:?}; registering anyway");
        }
        let (app, events) = register(&self.adapter).await?;
        self.app = Some(app);
        self.events = events;
        self.listening = true;
        Ok(())
    }

    pub fn has_listeners(&self) -> bool {
        self.listening
    }

    pub fn is_peer_connected(&self, peer: Address) -> bool {
        self.peers.get(&peer).is_some_and(|p| !p.subs.is_empty())
    }

    /// Wait until a host we have not seen before subscribes to an input
    /// report, and return its address. Later subscriptions from known hosts
    /// and closed sessions are absorbed here as bookkeeping. Cancel-safe: the
    /// state changes synchronously after each await returns.
    pub async fn accept_incoming(&mut self) -> Result<Address> {
        loop {
            tokio::select! {
                ev = self.events.next() => match ev {
                    Some((id, CharacteristicControlEvent::Notify(writer))) => {
                        if let Some(peer) = self.subscribe(id, writer) {
                            return Ok(peer);
                        }
                    }
                    Some((id, CharacteristicControlEvent::Write(req))) => {
                        // Input reports take no writes over the Io path.
                        debug!(peer = %req.device_address(), id, "rejecting write on input report");
                        req.reject(ReqError::NotSupported);
                    }
                    None => {
                        self.listening = false;
                        bail!("GATT control streams ended — HID application unregistered");
                    }
                },
                Some((peer, id, generation)) = self.closed_rx.recv() => {
                    self.forget_sub(peer, id, Some(generation), "host stopped notifications");
                }
            }
        }
    }

    /// Store a new subscription; `Some(peer)` when this is the peer's first.
    fn subscribe(&mut self, id: u8, writer: CharacteristicWriter) -> Option<Address> {
        let peer = writer.device_address();
        let is_new = !self.peers.contains_key(&peer);
        let generation = self.next_generation;
        self.next_generation += 1;
        info!(%peer, report_id = id, mtu = writer.mtu(), "input report subscribed");
        let writer = Arc::new(writer);
        let watcher = {
            let closed_tx = self.closed_tx.clone();
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                let _ = writer.closed().await;
                let _ = closed_tx.send((peer, id, generation));
            })
        };
        let entry = self.peers.entry(peer).or_default();
        entry.subs.insert(id, Sub { writer, generation, watcher });
        is_new.then_some(peer)
    }

    /// Drop one subscription (all of them when `generation` is `None`),
    /// and the peer once its last one is gone.
    fn forget_sub(&mut self, peer: Address, id: u8, generation: Option<u64>, why: &str) {
        let Some(entry) = self.peers.get_mut(&peer) else { return };
        let stale = entry
            .subs
            .get(&id)
            .is_some_and(|s| generation.is_none_or(|g| s.generation == g));
        if !stale {
            return;
        }
        entry.subs.remove(&id);
        debug!(%peer, report_id = id, why, "input report subscription dropped");
        if entry.subs.is_empty() {
            self.peers.remove(&peer);
            info!(%peer, "host detached (no input report subscriptions left)");
        }
    }

    /// Send a frame to `peer` (no-op with a debug log if it is not subscribed).
    pub async fn send_to(&mut self, peer: Address, frame: &HidFrame) -> Result<()> {
        let Some(entry) = self.peers.get_mut(&peer) else {
            debug!(%peer, "not subscribed, dropping frame");
            return Ok(());
        };
        let id = frame.report_id();
        let Some(sub) = entry.subs.get(&id) else {
            debug!(%peer, report_id = id, "host has not enabled this report, dropping frame");
            return Ok(());
        };
        let payload = frame.payload();
        let outcome = match sub.writer.try_send(payload) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if is_motion_only(frame, entry.last_mouse_buttons) {
                    debug!(%peer, "link busy, dropping motion-only report");
                    Ok(())
                } else {
                    match tokio::time::timeout(KEY_SEND_TIMEOUT, sub.writer.send(payload)).await {
                        Ok(res) => res,
                        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "send timed out")),
                    }
                }
            }
            Err(err) => Err(err),
        };
        match outcome {
            Ok(()) => {
                if let HidFrame::Mouse(b) = frame {
                    entry.last_mouse_buttons = b[1];
                }
            }
            Err(err) => {
                warn!(%peer, report_id = id, %err, "notification failed, dropping subscription");
                self.forget_sub(peer, id, None, "send failed");
            }
        }
        Ok(())
    }

    /// Disconnect every LE host and only then let the GATT application go.
    ///
    /// Dropping the application while a host is still connected makes
    /// bluetoothd send it *Service Changed*; a BlueZ host then rediscovers,
    /// finds only bluetoothd's own GAP/GATT/DeviceInfo services and rewrites
    /// its stored record without HID — and a device without an HID profile is
    /// never put on the kernel's auto-connect list, so after the next reboot
    /// (either side) it never comes back on its own. Cutting the link first
    /// leaves the host's cache intact; it reconnects as soon as we advertise
    /// again, with the application registered ahead of the advertisement.
    pub async fn shutdown(self) {
        let peers: Vec<Address> = match self.adapter.device_addresses().await {
            Ok(addrs) => addrs,
            Err(err) => {
                warn!(%err, "cannot list devices; unregistering HID application with hosts attached");
                return;
            }
        };
        let disconnects = peers.into_iter().filter_map(|peer| {
            let dev = self.adapter.device(peer).ok()?;
            Some(async move {
                if !dev.is_connected().await.unwrap_or(false) {
                    return;
                }
                info!(%peer, "disconnecting host before unregistering the HID application");
                // BlueZ replies to Disconnect once the link is actually down.
                match tokio::time::timeout(SHUTDOWN_DISCONNECT_TIMEOUT, dev.disconnect()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => debug!(%peer, %err, "disconnect failed"),
                    Err(_) => warn!(%peer, "disconnect timed out"),
                }
            })
        });
        futures::future::join_all(disconnects).await;
        // `self` (and with it `app`) drops here, after the links are gone.
    }

    /// Forget every subscription of `peer` and ask BlueZ to disconnect it.
    pub fn drop_peer(&mut self, peer: Address) {
        if self.peers.remove(&peer).is_some() {
            info!(%peer, "LE host dropped");
        }
        if let Ok(dev) = self.adapter.device(peer) {
            tokio::spawn(async move {
                if let Err(err) = dev.disconnect().await {
                    debug!(%peer, %err, "disconnect failed");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_information_is_v1_11_remote_wake_normally_connectable() {
        assert_eq!(hid_information_value(), vec![0x11, 0x01, 0x00, 0x03]);
    }

    #[test]
    fn pnp_id_is_usb_linux_foundation() {
        assert_eq!(pnp_id_value(), vec![0x02, 0x6B, 0x1D, 0x01, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn report_reference_encodes_id_and_type() {
        assert_eq!(report_reference(1, REPORT_TYPE_INPUT), vec![1, 1]);
        assert_eq!(report_reference(3, REPORT_TYPE_FEATURE), vec![3, 3]);
    }

    /// Read Blob hands us the offset of the next chunk; past the end is an
    /// error, exactly at the end is an empty (final) chunk.
    #[test]
    fn read_at_offset_slices_and_rejects_past_end() {
        let v = [1u8, 2, 3, 4];
        assert_eq!(read_at_offset(&v, 0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(read_at_offset(&v, 3).unwrap(), vec![4]);
        assert_eq!(read_at_offset(&v, 4).unwrap(), Vec::<u8>::new());
        assert_eq!(read_at_offset(&v, 5), Err(ReqError::InvalidOffset));
        assert_eq!(read_at_offset(REPORT_DESCRIPTOR, 22).unwrap().len(), REPORT_DESCRIPTOR.len() - 22);
    }

    #[test]
    fn motion_only_detection() {
        assert!(is_motion_only(&HidFrame::Mouse([2, 0, 5, 0, 0]), 0));
        assert!(!is_motion_only(&HidFrame::Mouse([2, 1, 0, 0, 0]), 0)); // press
        assert!(!is_motion_only(&HidFrame::Mouse([2, 0, 0, 0, 0]), 1)); // release
        assert!(!is_motion_only(&HidFrame::Keyboard([1, 0, 0, 0, 0, 0, 0, 0, 0]), 0));
        assert!(!is_motion_only(&HidFrame::Consumer([4, 0, 0]), 0));
    }

    #[test]
    fn advertisement_flags() {
        let adv = advertisement("KMHub", true);
        assert!(adv.service_uuids.contains(&Uuid::from_u16(SVC_HID)));
        assert_eq!(adv.appearance, Some(APPEARANCE_HID_KEYBOARD));
        assert_eq!(adv.local_name.as_deref(), Some("KMHub"));
        assert_eq!(adv.discoverable, Some(true));
        assert_eq!(advertisement("KMHub", false).discoverable, Some(false));
    }

    /// The table must agree with the report map: three input reports (IDs
    /// 1/2/4) with per-host `Io` notify and a Report Reference each, the
    /// battery feature report, and the two auxiliary services HOGP asks for.
    #[test]
    fn application_shape() {
        let (app, controls) = build_hid_application();
        let uuids: Vec<Uuid> = app.services.iter().map(|s| s.uuid).collect();
        assert_eq!(
            uuids,
            vec![Uuid::from_u16(SVC_HID), Uuid::from_u16(SVC_BATTERY), Uuid::from_u16(SVC_DEVICE_INFO)]
        );
        let ids: Vec<u8> = controls.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![REPORT_ID_KEYBOARD, REPORT_ID_MOUSE, REPORT_ID_CONSUMER]);

        let hid = &app.services[0];
        let reports: Vec<&Characteristic> =
            hid.characteristics.iter().filter(|c| c.uuid == Uuid::from_u16(CHR_REPORT)).collect();
        assert_eq!(reports.len(), 4, "3 input + 1 feature report");
        for c in &reports {
            let refs = c.descriptors.iter().filter(|d| d.uuid == Uuid::from_u16(DSC_REPORT_REFERENCE)).count();
            assert_eq!(refs, 1);
            assert!(c.read.as_ref().is_some_and(|r| r.encrypt_read));
        }
        let notifying = reports
            .iter()
            .filter(|c| matches!(c.notify, Some(CharacteristicNotify { method: CharacteristicNotifyMethod::Io, .. })))
            .count();
        assert_eq!(notifying, 3);
        assert!(hid.characteristics.iter().any(|c| c.uuid == Uuid::from_u16(CHR_REPORT_MAP)));
        assert!(hid.characteristics.iter().any(|c| c.uuid == Uuid::from_u16(CHR_HID_INFORMATION)));
        assert!(hid.characteristics.iter().any(|c| c.uuid == Uuid::from_u16(CHR_PROTOCOL_MODE)));
        assert!(hid.characteristics.iter().any(|c| c.uuid == Uuid::from_u16(CHR_HID_CONTROL_POINT)));
    }
}
