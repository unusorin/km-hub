//! Bluetooth lifecycle: adapter initialization, pairing agent, HID profile
//! (SDP record) registration, pairing windows with dynamic slot binding,
//! connection management and report streaming.

pub mod gatt;
pub mod sdp;
pub mod transport;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::adv::AdvertisementHandle;
use bluer::agent::{Agent, ReqError};
use bluer::rfcomm::{Profile, ProfileHandle, Role};
use bluer::{Adapter, Address};
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{LOCAL_SLOT, Settings};
use crate::hid::HidFrame;
use crate::rgb::RgbEvent;
use crate::state::{self, Binding, Bindings};
use gatt::LeTransport;
use transport::{L2capTransport, LogTransport, Transport};

/// `org.bluez.Device1.Bonded` — true only when a persistent link key exists.
/// A host that merely *connects* to us pairs Just-Works with "no bonding"
/// (kernel stores no key); BlueZ hosts then refuse HID from us
/// (`ClassicBondedOnly`). bluer 0.17 does not expose the property.
async fn is_bonded(adapter: &str, addr: Address) -> bool {
    let path = format!("/org/bluez/{adapter}/dev_{}", addr.to_string().replace(':', "_"));
    tokio::task::spawn_blocking(move || {
        use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
        let conn = dbus::blocking::Connection::new_system()?;
        conn.with_proxy("org.bluez", path, Duration::from_secs(2))
            .get::<bool>("org.bluez.Device1", "Bonded")
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .unwrap_or(false)
}

/// Class of device for a keyboard/mouse combo peripheral (major = Peripheral,
/// minor = keyboard+pointing). Only the major/minor bits (2..12) are compared:
/// bluetoothd ORs service-class bits (bits 13..23) in on its own.
const EXPECTED_CLASS: u32 = 0x0005C0;
const DEVICE_CLASS_MASK: u32 = 0x001FFC;

const HID_PROFILE_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";

const PAIRING_WINDOW: Duration = Duration::from_secs(60);
const REDIAL_COOLDOWN: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Log,
    L2cap,
    /// HID over GATT: LE peripheral, hosts connect to us. No classic HID
    /// listeners and no SDP record in this mode.
    Le,
}

/// Commands from the router.
#[derive(Debug)]
pub enum BtCmd {
    /// An unbound slot's combo was pressed: make the adapter discoverable and
    /// bind the next device that pairs to this slot.
    OpenPairingWindow { slot: u8 },
    /// The slot's combo was held down: forget whatever is bound there, drop the
    /// pairing, and open a window so the slot can be claimed again.
    RepairSlot { slot: u8 },
}

pub struct BtDeps {
    pub settings: Settings,
    pub kind: TransportKind,
    /// HID frames tagged with the slot they are meant for, so releases sent
    /// while switching still reach the host being left.
    pub frame_rx: mpsc::Receiver<(u8, HidFrame)>,
    pub cmd_rx: mpsc::Receiver<BtCmd>,
    pub slot_rx: watch::Receiver<u8>,
    pub bindings_tx: watch::Sender<Bindings>,
    pub bindings: Bindings,
    pub state_path: PathBuf,
    /// Pairing/bind notifications for the lighting task; `None` when lighting
    /// is disabled.
    pub rgb_tx: Option<mpsc::Sender<RgbEvent>>,
    pub cancel: CancellationToken,
}

struct PairingWindow {
    slot: u8,
    deadline: Instant,
    /// Devices already paired when the window opened.
    pre_paired: HashSet<Address>,
    /// A device has been bound; the adapter is no longer discoverable but
    /// stays pairable until `deadline` so the bond (link-key exchange) that
    /// may still be in flight on that connection completes with bonding.
    bound: bool,
}

struct Manager {
    adapter: Adapter,
    kind: TransportKind,
    settings: Settings,
    bindings: Bindings,
    bindings_tx: watch::Sender<Bindings>,
    state_path: PathBuf,
    pairing: Option<PairingWindow>,
    rgb_tx: Option<mpsc::Sender<RgbEvent>>,
    /// The LE advertisement (LE mode only). Always registered while running:
    /// connectable so bonded hosts can reconnect, discoverable only while a
    /// pairing window is open. Re-registered to flip that flag, since BlueZ
    /// reads an advertisement's properties once.
    adv: Option<AdvertisementHandle>,
}

/// bluer unregisters a dropped advertisement from a spawned task; give it a
/// moment before registering the replacement or the two overlap.
const ADV_REREGISTER_DELAY: Duration = Duration::from_millis(250);

pub async fn run(deps: BtDeps) -> Result<()> {
    let BtDeps {
        settings,
        kind,
        mut frame_rx,
        mut cmd_rx,
        mut slot_rx,
        bindings_tx,
        bindings,
        state_path,
        rgb_tx,
        cancel,
    } = deps;

    let session = bluer::Session::new()
        .await
        .context("cannot connect to BlueZ (is bluetooth.service running?)")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;

    adapter.set_powered(true).await?;
    adapter.set_alias(settings.adapter_alias.clone()).await?;
    // Visible/pairable only during an explicit pairing window; also cleans up
    // whatever a previous run left behind.
    adapter.set_discoverable(false).await?;
    adapter.set_pairable(false).await?;
    let addr = adapter.address().await?;
    info!(
        adapter = %adapter.name(),
        %addr,
        alias = %settings.adapter_alias,
        "adapter powered (discoverable only during pairing windows)"
    );

    // Class of device is a BR/EDR notion; on LE the advertisement's
    // appearance plays that role.
    if kind != TransportKind::Le {
        match adapter.class().await {
            Ok(class) if class & DEVICE_CLASS_MASK != EXPECTED_CLASS => warn!(
                class = format!("{class:#08x}"),
                "adapter class is not keyboard/mouse combo ({EXPECTED_CLASS:#08x}) — \
                 macOS may misclassify us; run ./setup.sh (main.conf Class step)"
            ),
            Ok(_) => {}
            Err(err) => debug!(%err, "cannot read adapter class"),
        }
    }

    // Pairing agent: Just-Works style auto-accept, with every callback logged
    // so a macOS pairing attempt tells us which flow it actually uses.
    let _agent_handle = session.register_agent(make_agent()).await?;
    info!("pairing agent registered (auto-accept)");

    // Publish the HID SDP record (classic modes only — an LE host discovers
    // us through GATT, and an SDP record would invite a dual-mode host to try
    // the classic transport we don't serve). Actual connections arrive on our
    // own L2CAP listeners; ProfileManager connect requests are logged only.
    let mut profile_handle: Option<ProfileHandle> = if kind == TransportKind::Le {
        None
    } else {
        let profile = Profile {
            uuid: HID_PROFILE_UUID.parse().unwrap(),
            name: Some(settings.adapter_alias.clone()),
            role: Some(Role::Server),
            require_authentication: Some(false),
            require_authorization: Some(false),
            auto_connect: Some(true),
            service_record: Some(sdp::hid_service_record(&settings.adapter_alias)),
            ..Default::default()
        };
        let handle = session
            .register_profile(profile)
            .await
            .context("HID profile registration failed")?;
        info!("HID profile (0x1124) registered with SDP record");
        Some(handle)
    };

    let mut transport = match kind {
        TransportKind::Log => {
            info!("using log transport (frames are printed, not sent)");
            Transport::Log(LogTransport)
        }
        TransportKind::L2cap => Transport::L2cap(L2capTransport::bind(addr).await?),
        TransportKind::Le => Transport::Le(LeTransport::serve(adapter.clone()).await?),
    };

    let mut mgr = Manager {
        adapter,
        kind,
        settings,
        bindings,
        bindings_tx,
        state_path,
        pairing: None,
        rgb_tx,
        adv: None,
    };
    if let Some((slot, b)) = mgr.bindings.iter().next_back() {
        debug!(slots = mgr.bindings.len(), last_slot = slot, last = %b.name, "loaded slot bindings");
    }
    if kind == TransportKind::Le {
        // Hosts connect to us on LE: advertise from the start (connectable,
        // not discoverable) so bonded ones can come back on their own.
        mgr.set_advertising(false).await?;
    }

    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // After a failed dial, wait before retrying so a host that rejects us
    // doesn't cause a reconnect storm.
    let mut redial_at: Option<Instant> = None;
    // The in-flight outgoing connect. It runs as its own task so that other
    // select! branches firing (frames, ticks) never drop and restart it —
    // an aborted Create Connection every few ms wedges controllers.
    let mut dial_task: Option<(Address, JoinHandle<Result<DialOutcome>>)> = None;

    loop {
        // Every bound host may stay connected; we only *dial* the active
        // slot's host when it isn't (classic only — LE hosts dial us). Frames
        // are routed by their own slot tag.
        let active_slot = *slot_rx.borrow();
        let dial = if transport.can_dial() { mgr.dial_target(active_slot) } else { None };
        if let Some((addr, task)) = &dial_task
            && dial != Some(*addr)
        {
            // Target changed mid-dial: abandon it.
            task.abort();
            dial_task = None;
        }
        let need_dial = dial.is_some_and(|a| !transport.is_peer_connected(a));
        if let (true, None, Some(addr)) = (need_dial, &dial_task, dial)
            && redial_at.is_none_or(|t| Instant::now() >= t)
        {
            let adapter = mgr.adapter.clone();
            dial_task = Some((addr, tokio::spawn(async move { dial_active(addr, &adapter).await })));
        }
        let backoff_until = redial_at.unwrap_or_else(Instant::now);
        let wait_backoff = need_dial && dial_task.is_none() && redial_at.is_some();
        let dial_pending = async {
            match &mut dial_task {
                Some((_, task)) => task.await,
                None => futures::future::pending().await,
            }
        };
        let profile_request = async {
            match &mut profile_handle {
                Some(handle) => handle.next().await,
                None => futures::future::pending().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => break,
            res = dial_pending => {
                let (addr, _) = dial_task.take().expect("dial task was pending");
                match res {
                    Ok(Ok(DialOutcome::Connected(streams))) => {
                        redial_at = None;
                        transport.add_outgoing(addr, streams);
                    }
                    Ok(Ok(DialOutcome::StaleBinding)) => {
                        redial_at = None;
                        mgr.heal_stale_binding(active_slot).await;
                    }
                    Ok(Err(err)) => {
                        debug!(err = format!("{err:#}"), "outgoing connect failed, backing off");
                        redial_at = Some(Instant::now() + REDIAL_COOLDOWN);
                    }
                    Err(join_err) => {
                        warn!(%join_err, "dial task panicked");
                        redial_at = Some(Instant::now() + REDIAL_COOLDOWN);
                    }
                }
            }
            _ = tokio::time::sleep_until(backoff_until), if wait_backoff => redial_at = None,
            res = transport.accept_incoming(), if transport.has_listeners() => match res {
                Ok(peer) => mgr.on_incoming_peer(&mut transport, peer).await,
                Err(err) => warn!(%err, "incoming accept failed"),
            },
            msg = frame_rx.recv() => match msg {
                Some((slot, frame)) => match mgr.bindings.get(&slot).map(|b| b.addr) {
                    Some(peer) => transport.send_to(peer, &frame).await?,
                    None => debug!(slot, "frame for unbound slot, dropping"),
                },
                None => break,
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => mgr.handle_cmd(cmd).await,
                None => break,
            },
            _ = poll.tick() => mgr.poll_window().await,
            _ = slot_rx.changed() => {}
            req = profile_request => log_connect_request(req),
        }
    }

    mgr.close_window().await;
    // Dropping the handle unregisters the advertisement; no re-advertising on
    // the way out.
    mgr.adv.take();
    info!("bluetooth manager stopped");
    Ok(())
}

impl Manager {
    /// Lighting is cosmetic: never wait on it, and drop the event if the
    /// task is behind.
    fn notify_rgb(&self, event: RgbEvent) {
        if let Some(tx) = &self.rgb_tx
            && tx.try_send(event).is_err()
        {
            debug!(?event, "lighting task not accepting events");
        }
    }

    /// Make the hub visible to new hosts (pairing window) or not. Classic:
    /// the adapter's discoverable flag (inquiry scan). LE: the discoverable
    /// flag of our advertisement — not the adapter property, which on a
    /// dual-mode controller would also open BR/EDR inquiry scan.
    async fn set_visible(&mut self, on: bool) -> bluer::Result<()> {
        match self.kind {
            TransportKind::Log | TransportKind::L2cap => self.adapter.set_discoverable(on).await,
            TransportKind::Le => self.set_advertising(on).await,
        }
    }

    /// (Re)register the LE advertisement with the given discoverable flag.
    async fn set_advertising(&mut self, discoverable: bool) -> bluer::Result<()> {
        if self.adv.take().is_some() {
            tokio::time::sleep(ADV_REREGISTER_DELAY).await;
        }
        let adv = gatt::advertisement(&self.settings.adapter_alias, discoverable);
        self.adv = Some(self.adapter.advertise(adv).await?);
        info!(discoverable, "LE advertising (connectable HID peripheral)");
        Ok(())
    }

    fn dial_target(&self, active_slot: u8) -> Option<Address> {
        if active_slot == LOCAL_SLOT {
            return None;
        }
        self.bindings.get(&active_slot).map(|b| b.addr)
    }

    async fn handle_cmd(&mut self, cmd: BtCmd) {
        match cmd {
            BtCmd::OpenPairingWindow { slot } => self.open_window(slot).await,
            BtCmd::RepairSlot { slot } => self.repair_slot(slot).await,
        }
    }

    /// Forced re-pair (long press on the slot's combo): unbind the slot, remove
    /// the device from BlueZ so no stale link key survives, then open a pairing
    /// window. The *client* keeps its own key, so a clean re-pair also needs the
    /// device forgotten there — logged as a reminder.
    async fn repair_slot(&mut self, slot: u8) {
        match self.bindings.remove(&slot) {
            Some(binding) => {
                info!(
                    slot,
                    addr = %binding.addr,
                    name = %binding.name,
                    "re-pair requested — unbinding and removing the pairing"
                );
                if binding.from_config {
                    warn!(
                        slot,
                        "binding was pre-seeded from config.toml — remove its 'mac' there or it returns on restart"
                    );
                }
                if let Ok(dev) = self.adapter.device(binding.addr) {
                    dev.disconnect().await.ok();
                }
                if let Err(err) = self.adapter.remove_device(binding.addr).await {
                    warn!(slot, addr = %binding.addr, %err, "cannot remove the pairing");
                }
                warn!(
                    slot,
                    name = %binding.name,
                    "now forget this hub on the client too, then pair again while the window is open"
                );
                let _ = self.bindings_tx.send(self.bindings.clone());
                state::save(&self.state_path, &self.bindings);
            }
            None => {
                // The plain press already opened a window for this slot; the
                // hold that followed has nothing left to undo.
                if self.pairing.as_ref().is_some_and(|w| w.slot == slot) {
                    debug!(slot, "re-pair requested, pairing window already open");
                    return;
                }
                info!(slot, "re-pair requested on an unbound slot");
            }
        }
        self.open_window(slot).await;
    }

    /// The active slot's device is no longer paired in BlueZ: drop the dead
    /// binding and go straight back into pairing mode so the user can re-pair
    /// without touching state.toml.
    async fn heal_stale_binding(&mut self, slot: u8) {
        let Some(binding) = self.bindings.get(&slot) else { return };
        warn!(
            slot,
            addr = %binding.addr,
            name = %binding.name,
            "bound device is no longer paired — unbinding and reopening pairing window"
        );
        if binding.from_config {
            warn!(slot, "binding was pre-seeded from config.toml — remove its 'mac' there or it returns on restart");
        }
        self.bindings.remove(&slot);
        let _ = self.bindings_tx.send(self.bindings.clone());
        state::save(&self.state_path, &self.bindings);
        self.open_window(slot).await;
    }

    async fn open_window(&mut self, slot: u8) {
        if self.bindings.contains_key(&slot) {
            info!(slot, "slot already bound — ignoring pairing request");
            return;
        }
        if let Some(w) = &self.pairing {
            info!(old_slot = w.slot, new_slot = slot, "replacing open pairing window");
        }
        let mut pre_paired = HashSet::new();
        if let Ok(addrs) = self.adapter.device_addresses().await {
            for addr in addrs {
                if let Ok(dev) = self.adapter.device(addr)
                    && dev.is_paired().await.unwrap_or(false)
                {
                    pre_paired.insert(addr);
                }
            }
        }
        if let Err(err) = self.adapter.set_pairable(true).await {
            warn!(%err, "cannot make adapter pairable");
            return;
        }
        if let Err(err) = self.set_visible(true).await {
            warn!(%err, "cannot make adapter discoverable");
            return;
        }
        self.pairing = Some(PairingWindow {
            slot,
            deadline: Instant::now() + PAIRING_WINDOW,
            pre_paired,
            bound: false,
        });
        info!(
            slot,
            window_secs = PAIRING_WINDOW.as_secs(),
            "pairing window open — PAIR (not just connect) from the new device now"
        );
        self.notify_rgb(RgbEvent::PairingOpen);
    }

    async fn close_window(&mut self) {
        if let Some(window) = self.pairing.take() {
            // A settled window already turned visibility off.
            if !window.bound && let Err(err) = self.set_visible(false).await {
                warn!(%err, "cannot leave discoverable mode");
            }
            self.adapter.set_pairable(false).await.ok();
            self.notify_rgb(RgbEvent::PairingClosed);
        }
    }

    /// A device was bound: stop advertising, but keep the adapter pairable
    /// for the rest of the window (see `PairingWindow::bound`).
    async fn settle_window(&mut self) {
        if let Some(w) = &mut self.pairing {
            w.bound = true;
            if let Err(err) = self.set_visible(false).await {
                warn!(%err, "cannot leave discoverable mode");
            }
        }
    }

    /// Once per second while a window is open: bind the first newly paired
    /// device, or time the window out.
    async fn poll_window(&mut self) {
        let Some(window) = &self.pairing else { return };
        let (slot, deadline, bound) = (window.slot, window.deadline, window.bound);
        if Instant::now() >= deadline {
            if !bound {
                info!(slot, "pairing window timed out — no device paired");
            }
            self.close_window().await;
            return;
        }
        if bound {
            return;
        }
        let Ok(addrs) = self.adapter.device_addresses().await else { return };
        let mut newly_paired = None;
        for addr in addrs {
            let skip = self
                .pairing
                .as_ref()
                .is_some_and(|w| w.pre_paired.contains(&addr))
                || self.bindings.values().any(|b| b.addr == addr);
            if skip {
                continue;
            }
            if let Ok(dev) = self.adapter.device(addr)
                && dev.is_paired().await.unwrap_or(false)
                && is_bonded(self.adapter.name(), addr).await
            {
                newly_paired = Some(addr);
                break;
            }
        }
        if let Some(addr) = newly_paired {
            self.bind(slot, addr).await;
            self.settle_window().await;
        }
    }

    /// A host connected to our listeners. Known (bound) hosts are simply kept
    /// attached. During a pairing window a new *bonded* host gets bound to the
    /// window's slot — the strongest signal we have that this is the device
    /// the user meant. A host that only "connected" (Just-Works, no stored
    /// key) would reject our HID traffic, so drop it and ask for a real Pair.
    /// Unknown hosts outside a window are dropped.
    async fn on_incoming_peer(&mut self, transport: &mut Transport, peer: Address) {
        if self.bindings.values().any(|b| b.addr == peer) {
            return;
        }
        let Some(window) = &self.pairing else {
            info!(%peer, "connection from an unbound host outside a pairing window — dropping");
            transport.drop_peer(peer);
            return;
        };
        if window.bound {
            info!(%peer, "another host connected during a settled window — dropping");
            transport.drop_peer(peer);
            return;
        }
        let slot = window.slot;
        if !is_bonded(self.adapter.name(), peer).await {
            warn!(
                slot,
                %peer,
                "client connected without bonding (it used Connect, not Pair) — \
                 dropping it. On the client: forget/remove this device, then PAIR it \
                 (confirm the passkey); the pairing window stays open"
            );
            transport.drop_peer(peer);
            if let Ok(dev) = self.adapter.device(peer) {
                dev.disconnect().await.ok();
            }
            self.adapter.remove_device(peer).await.ok();
            return;
        }
        self.bind(slot, peer).await;
        self.settle_window().await;
    }

    async fn bind(&mut self, slot: u8, addr: Address) {
        let device_name = match self.adapter.device(addr) {
            Ok(dev) => {
                // Trusted devices may reconnect without re-authorization.
                if let Err(err) = dev.set_trusted(true).await {
                    debug!(%err, "cannot mark device trusted");
                }
                dev.alias().await.unwrap_or_else(|_| addr.to_string())
            }
            Err(_) => addr.to_string(),
        };
        // A mac-less config target pre-names the slot; otherwise use the
        // device's own name.
        let name = self
            .settings
            .target_for_slot(slot)
            .map(|t| t.name.clone())
            .unwrap_or(device_name);
        info!(slot, %addr, name, "device bound to slot");
        self.bindings.insert(
            slot,
            Binding {
                addr,
                name,
                from_config: false,
            },
        );
        let _ = self.bindings_tx.send(self.bindings.clone());
        state::save(&self.state_path, &self.bindings);
        self.notify_rgb(RgbEvent::Bound);
    }
}

enum DialOutcome {
    Connected((bluer::l2cap::Stream, bluer::l2cap::Stream)),
    /// The active slot's device is no longer paired in BlueZ (pairing removed
    /// out-of-band) — dialing it can never succeed.
    StaleBinding,
}

/// One device-initiated connection attempt to the active slot's host.
async fn dial_active(addr: Address, adapter: &Adapter) -> Result<DialOutcome> {
    // A device whose pairing was removed will never accept our HID
    // connection; surface it so the binding can be healed.
    let paired = match adapter.device(addr) {
        Ok(dev) => dev.is_paired().await.unwrap_or(false),
        Err(_) => false,
    };
    if !paired {
        return Ok(DialOutcome::StaleBinding);
    }
    Ok(DialOutcome::Connected(L2capTransport::dial(addr).await?))
}

fn log_connect_request(req: Option<bluer::rfcomm::ConnectRequest>) {
    match req {
        // TODO(stub): wire the request's fd into the transport instead of
        // dropping it (drop = reject). Connections currently arrive via our
        // own L2CAP listeners, not via ProfileManager.
        Some(req) => info!(device = %req.device(), "profile connect request (ignored, stub)"),
        None => debug!("profile handle stream ended"),
    }
}

fn make_agent() -> Agent {
    Agent {
        request_default: true,
        request_pin_code: Some(Box::new(|req| {
            async move {
                info!(device = %req.device, "pairing: pin code requested, sending 0000");
                Ok("0000".to_string())
            }
            .boxed()
        })),
        display_pin_code: Some(Box::new(|req| {
            async move {
                info!(device = %req.device, pin = %req.pincode, "pairing: display pin code");
                Ok(())
            }
            .boxed()
        })),
        request_passkey: Some(Box::new(|req| {
            async move {
                // Passkey-entry pairing needs the HID transport to "type" the
                // digits — not supported yet (see config.example.toml caveats).
                warn!(device = %req.device, "pairing: passkey entry requested — unsupported, rejecting");
                Err(ReqError::Rejected)
            }
            .boxed()
        })),
        display_passkey: Some(Box::new(|req| {
            async move {
                info!(device = %req.device, passkey = req.passkey, "pairing: display passkey");
                Ok(())
            }
            .boxed()
        })),
        request_confirmation: Some(Box::new(|req| {
            async move {
                info!(device = %req.device, passkey = req.passkey, "pairing: auto-confirming");
                Ok(())
            }
            .boxed()
        })),
        request_authorization: Some(Box::new(|req| {
            async move {
                info!(device = %req.device, "pairing: auto-authorizing device");
                Ok(())
            }
            .boxed()
        })),
        authorize_service: Some(Box::new(|req| {
            async move {
                info!(device = %req.device, service = %req.service, "authorizing service");
                Ok(())
            }
            .boxed()
        })),
        ..Default::default()
    }
}
