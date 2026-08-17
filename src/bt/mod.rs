//! Bluetooth lifecycle: adapter initialization, pairing agent, HID profile
//! (SDP record) registration, pairing windows with dynamic slot binding,
//! connection management and report streaming.

pub mod gatt;
pub mod replay;
pub mod sdp;
pub mod transport;

use std::collections::{HashMap, HashSet};
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

use crate::config::{Connections, LOCAL_SLOT, Settings};
use crate::hid::HidFrame;
use crate::rgb::RgbEvent;
use crate::state::{self, Binding, Bindings};
use gatt::LeTransport;
use replay::{Push, ReplayBuffer};
use transport::{Incoming, L2capTransport, LogTransport, Transport};

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

/// The active slot's host between connections (single-connection mode):
/// frames for it are held here until it subscribes to their report.
struct Pending {
    addr: Address,
    buffer: ReplayBuffer,
    since: Instant,
    /// Frames replayed so far, for the summary once the host is attached.
    replayed: u32,
}

struct Manager {
    adapter: Adapter,
    kind: TransportKind,
    settings: Settings,
    bindings: Bindings,
    bindings_tx: watch::Sender<Bindings>,
    state_path: PathBuf,
    pairing: Option<PairingWindow>,
    /// Bound hosts seen connected without an input-report session, and since
    /// when (see `poll_stuck_hosts`).
    stuck: HashMap<Address, Instant>,
    /// Stuck hosts we disconnected so they reconnect fresh, and when. A host
    /// stuck again within `KICK_MEMORY` of its kick escalates to a GATT
    /// re-registration; forgotten once the host subscribes.
    kicked: HashMap<Address, Instant>,
    /// Hosts already served by a re-registration during their current
    /// connection; forgotten when they disconnect. Bounds the churn to one
    /// re-registration per host connection.
    helped: HashSet<Address>,
    last_reregister: Option<Instant>,
    rgb_tx: Option<mpsc::Sender<RgbEvent>>,
    /// The LE advertisement (LE mode only): connectable so bonded hosts can
    /// reconnect, discoverable only while a pairing window is open, and
    /// dropped while nobody is expected (`apply_advertising`). Re-registered
    /// to flip the discoverable flag, since BlueZ reads an advertisement's
    /// properties once; `adv_discoverable` remembers which flavour is up.
    adv: Option<AdvertisementHandle>,
    adv_discoverable: bool,
    /// Mirror of the router's slot watch; the manager needs it outside the
    /// loop prelude (incoming hosts, advertising, eviction).
    active_slot: u8,
    /// See `Pending`. `None` when the active host is attached, the active
    /// slot is local, or the mode keeps every host connected.
    pending: Option<Pending>,
    /// Bound hosts told to disconnect because their slot is not active, and
    /// when — so a host that lingers through bluetoothd's disconnect timer
    /// is not told again every tick.
    evicted: HashMap<Address, Instant>,
}

/// A bound host connected this long without any input-report session is
/// taken to be stuck behind bluetoothd's remembered CCC state. Reconnecting
/// hosts subscribe well inside a second; a full rediscovery took ~1.5 s.
const STUCK_HOST_GRACE: Duration = Duration::from_secs(3);
/// How long a kicked host stays "already kicked": stuck again inside this
/// window means the reconnect did not help and the CCC state must be cleared
/// by re-registering. Hosts reconnect within a second or two; the slack is
/// for phones that take their time.
const KICK_MEMORY: Duration = Duration::from_secs(30);
/// Minimum spacing between two GATT re-registrations.
const REREGISTER_COOLDOWN: Duration = Duration::from_secs(10);

/// bluer unregisters a dropped advertisement from a spawned task; give it a
/// moment before registering the replacement or the two overlap.
const ADV_REREGISTER_DELAY: Duration = Duration::from_millis(250);

/// How long frames are held for a host that has not come back after a slot
/// switch. Reconnects take a second or two, a stuck reconnect one kick
/// (`STUCK_HOST_GRACE`) more; after this the buffered input is stale and is
/// dropped (with its counts logged).
const REPLAY_WINDOW: Duration = Duration::from_secs(8);
/// Minimum spacing between two "disconnect, your slot is not active" nudges
/// to the same host; bluetoothd takes up to 2 s to actually drop the link.
const EVICT_COOLDOWN: Duration = Duration::from_secs(5);

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
        stuck: HashMap::new(),
        kicked: HashMap::new(),
        helped: HashSet::new(),
        last_reregister: None,
        rgb_tx,
        adv: None,
        adv_discoverable: false,
        active_slot: *slot_rx.borrow(),
        pending: None,
        evicted: HashMap::new(),
    };
    if let Some((slot, b)) = mgr.bindings.iter().next_back() {
        debug!(slots = mgr.bindings.len(), last_slot = slot, last = %b.name, "loaded slot bindings");
    }
    if kind == TransportKind::Le {
        info!(connections = ?mgr.settings.connections, slot = mgr.active_slot, "LE hosts connect to us");
        // Hosts connect to us on LE: advertise from the start (connectable,
        // not discoverable) for whoever is expected — every bound host, or
        // just the active slot's — so it comes back on its own; the poll
        // turns it off once it has (see `apply_advertising`).
        let connected = mgr.connected_hosts().await;
        mgr.apply_advertising(&connected).await;
        mgr.arm_pending(&transport);
    } else if mgr.settings.connections == Connections::Single {
        debug!("connections = \"single\" applies to the LE transport only");
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
        // Classic: every bound host may stay connected; we only *dial* the
        // active slot's host when it isn't. LE hosts dial us (see
        // `on_slot_change` for what a switch means there). Frames are routed
        // by their own slot tag.
        let active_slot = mgr.active_slot;
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
                Ok(Incoming::Connected(peer)) => mgr.on_incoming_peer(&mut transport, peer).await,
                Ok(Incoming::Subscribed { peer, report_id, first }) => {
                    if first {
                        mgr.on_incoming_peer(&mut transport, peer).await;
                    }
                    mgr.on_subscribed(&mut transport, peer, report_id).await?;
                }
                Err(err) => warn!(%err, "incoming accept failed"),
            },
            msg = frame_rx.recv() => match msg {
                Some((slot, frame)) => {
                    // A frame for another slot can overtake the slot watch in
                    // this select!: the router tags frames with the slot it
                    // has already switched to. Take the switch first, so the
                    // frame is held for the new host instead of being sent to
                    // one that is not subscribed. (Frames for the slot being
                    // left were queued before the watch changed and never
                    // trip this.)
                    if slot != mgr.active_slot && slot_rx.has_changed().unwrap_or(false) {
                        let new = *slot_rx.borrow_and_update();
                        mgr.on_slot_change(&mut transport, &mut frame_rx, new).await?;
                    }
                    mgr.route_frame(&mut transport, slot, frame).await?
                }
                None => break,
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => mgr.handle_cmd(cmd).await,
                None => break,
            },
            _ = poll.tick() => {
                mgr.poll_window().await;
                let connected = mgr.connected_hosts().await;
                mgr.poll_uninvited(&mut transport, &connected);
                mgr.apply_advertising(&connected).await;
                mgr.poll_stuck_hosts(&mut transport, &connected).await;
                mgr.poll_pending();
            }
            _ = slot_rx.changed() => {
                let slot = *slot_rx.borrow_and_update();
                mgr.on_slot_change(&mut transport, &mut frame_rx, slot).await?;
            }
            req = profile_request => log_connect_request(req),
        }
    }

    mgr.close_window().await;
    // Dropping the handle unregisters the advertisement; no re-advertising on
    // the way out. Do this before dropping the hosts so none reconnects into
    // a GATT application that is about to disappear.
    mgr.adv.take();
    transport.shutdown().await;
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
            TransportKind::Le if on => self.set_advertising(true).await,
            TransportKind::Le => {
                // Back to whatever the mode wants: a connectable advertisement
                // for the hosts still expected, or none at all.
                let connected = self.connected_hosts().await;
                self.apply_advertising(&connected).await;
                Ok(())
            }
        }
    }

    /// (Re)register the LE advertisement with the given discoverable flag.
    async fn set_advertising(&mut self, discoverable: bool) -> bluer::Result<()> {
        if self.adv.take().is_some() {
            tokio::time::sleep(ADV_REREGISTER_DELAY).await;
        }
        let adv = gatt::advertisement(&self.settings.adapter_alias, discoverable);
        self.adv = Some(self.adapter.advertise(adv).await?);
        self.adv_discoverable = discoverable;
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
                state::save(&self.state_path, &self.bindings, self.active_slot);
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
        state::save(&self.state_path, &self.bindings, self.active_slot);
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

    /// Bound hosts bluetoothd currently shows connected.
    async fn connected_hosts(&self) -> HashSet<Address> {
        let mut out = HashSet::new();
        for b in self.bindings.values() {
            if let Ok(dev) = self.adapter.device(b.addr)
                && dev.is_connected().await.unwrap_or(false)
            {
                out.insert(b.addr);
            }
        }
        out
    }

    /// LE with `connections = "single"`: one host at a time.
    fn single(&self) -> bool {
        self.kind == TransportKind::Le && self.settings.connections == Connections::Single
    }

    /// The host frames go to right now; none while the local slot is active
    /// or the active slot is unbound.
    fn active_host(&self) -> Option<Address> {
        (self.active_slot != LOCAL_SLOT)
            .then(|| self.bindings.get(&self.active_slot).map(|b| b.addr))
            .flatten()
    }

    fn slot_of(&self, addr: Address) -> Option<u8> {
        self.bindings.iter().find(|(_, b)| b.addr == addr).map(|(&slot, _)| slot)
    }

    /// Whether this slot's host is one we keep connected: every bound host,
    /// or in single mode only the active one.
    fn serves(&self, slot: u8) -> bool {
        !self.single() || slot == self.active_slot
    }

    /// Bring the advertisement in line with who is expected to connect (see
    /// `wants_advertising`), unless an unsettled pairing window owns it: that
    /// one is discoverable, and `open_window`/`close_window` handle it.
    /// Called from the 1 s tick, on a slot switch (so the target's reconnect
    /// starts at once) and when a window settles or closes.
    async fn apply_advertising(&mut self, connected: &HashSet<Address>) {
        if self.kind != TransportKind::Le || self.pairing.as_ref().is_some_and(|w| !w.bound) {
            return;
        }
        let bound: Vec<Address> = self.bindings.values().map(|b| b.addr).collect();
        let want = wants_advertising(self.settings.connections, self.active_host(), &bound, connected);
        match (want, self.adv.is_some()) {
            (true, false) => {
                if let Err(err) = self.set_advertising(false).await {
                    warn!(%err, "cannot start advertising");
                }
            }
            (true, true) if self.adv_discoverable => {
                // A settled pairing window: stay connectable, stop being seen.
                if let Err(err) = self.set_advertising(false).await {
                    warn!(%err, "cannot leave discoverable mode");
                }
            }
            (false, true) => {
                self.adv.take();
                info!("expected hosts connected; advertising off");
            }
            _ => {}
        }
    }

    /// Hold frames for the active host until it subscribes (single mode
    /// only, and only if it is not attached already). Called after a slot
    /// switch and at startup.
    fn arm_pending(&mut self, transport: &Transport) {
        self.pending = None;
        if !self.single() {
            return;
        }
        let Some(addr) = self.active_host() else { return };
        if gatt::INPUT_REPORT_IDS.iter().all(|&id| transport.is_subscribed(addr, id)) {
            return;
        }
        self.pending = Some(Pending {
            addr,
            buffer: ReplayBuffer::new(),
            since: Instant::now(),
            replayed: 0,
        });
    }

    /// The router moved to another slot. Classic and "all" mode: remember
    /// it (the classic loop dials from it). Single mode, the channel switch:
    ///
    /// 1. flush what the router already queued — its `release_all` frames for
    ///    the host we are leaving went into `frame_rx` *before* the slot
    ///    watch changed, and mpsc keeps that order, so they still find the
    ///    old host subscribed;
    /// 2. hold frames for the new host until it subscribes;
    /// 3. advertise now rather than at the next tick, so the new host's
    ///    reconnect starts immediately;
    /// 4. disconnect the old host. bluetoothd defers the actual link drop by
    ///    up to 2 s (bluetoothd's DISCONNECT_TIMER, see `LeTransport::
    ///    shutdown`), so the release notifications queued in (1) go out first.
    async fn on_slot_change(
        &mut self,
        transport: &mut Transport,
        frame_rx: &mut mpsc::Receiver<(u8, HidFrame)>,
        slot: u8,
    ) -> Result<()> {
        let old_host = self.active_host();
        self.active_slot = slot;
        state::save(&self.state_path, &self.bindings, self.active_slot);
        if !self.single() {
            return Ok(());
        }
        while let Ok((slot, frame)) = frame_rx.try_recv() {
            self.route_frame(transport, slot, frame).await?;
        }
        if let Some(p) = self.pending.take()
            && !p.buffer.is_empty()
        {
            let (dropped_motion, evicted) = p.buffer.stats();
            debug!(peer = %p.addr, held = p.buffer.len(), dropped_motion, evicted, "switched away before the host came back; held frames dropped");
        }
        self.arm_pending(transport);
        let new_host = self.active_host();
        let connected = self.connected_hosts().await;
        self.apply_advertising(&connected).await;
        if let Some(old) = old_host
            && Some(old) != new_host
        {
            info!(peer = %old, slot = self.slot_of(old), "slot switch — disconnecting the previous host");
            transport.drop_peer(old);
            self.forget_host(old);
            // bluetoothd shows it connected for up to 2 s more; the eviction
            // sweep need not tell it again.
            self.evicted.insert(old, Instant::now());
        }
        Ok(())
    }

    /// Drop every per-connection note about a host we just disconnected.
    fn forget_host(&mut self, addr: Address) {
        self.stuck.remove(&addr);
        self.kicked.remove(&addr);
        self.helped.remove(&addr);
        self.evicted.remove(&addr);
    }

    /// Send a frame to its slot's host, or hold it if that host is the one
    /// we are waiting for and has not enabled this report yet.
    async fn route_frame(&mut self, transport: &mut Transport, slot: u8, frame: HidFrame) -> Result<()> {
        let Some(peer) = self.bindings.get(&slot).map(|b| b.addr) else {
            debug!(slot, "frame for unbound slot, dropping");
            return Ok(());
        };
        if let Some(p) = &mut self.pending
            && p.addr == peer
            && !transport.is_subscribed(peer, frame.report_id())
        {
            match p.buffer.push(frame) {
                Push::Queued => {}
                Push::DroppedMotion => {}
                Push::Evicted => debug!(%peer, "replay buffer full — oldest held frame dropped"),
            }
            return Ok(());
        }
        transport.send_to(peer, &frame).await
    }

    /// A host enabled one of its input reports: if it is the one we are
    /// holding frames for, replay that report's frames now, and once all
    /// three reports are enabled, stop holding.
    async fn on_subscribed(&mut self, transport: &mut Transport, peer: Address, report_id: u8) -> Result<()> {
        let Some(p) = &mut self.pending else { return Ok(()) };
        // `on_incoming_peer` may just have dropped an uninvited host.
        if p.addr != peer || !transport.is_subscribed(peer, report_id) {
            return Ok(());
        }
        let frames = p.buffer.take(report_id);
        if !frames.is_empty() {
            debug!(%peer, report_id, replayed = frames.len(), "replaying frames held during the reconnect");
        }
        p.replayed += frames.len() as u32;
        for frame in &frames {
            transport.send_to(peer, frame).await?;
        }
        if gatt::INPUT_REPORT_IDS.iter().all(|&id| transport.is_subscribed(peer, id)) {
            let p = self.pending.take().expect("checked above");
            let (dropped_motion, evicted) = p.buffer.stats();
            debug!(
                %peer,
                gap_ms = p.since.elapsed().as_millis(),
                replayed = p.replayed,
                dropped_motion,
                evicted,
                "host attached after the switch"
            );
        }
        Ok(())
    }

    /// Single mode, once a second: a bound host whose slot is not active but
    /// which is connected anyway (it woke up and saw our advertisement, or is
    /// left over from before a restart) is told to go — unless a pairing
    /// window is open, which is how a host just bound to another slot looks.
    /// Both notions of connected are checked because bluetoothd's
    /// `Connected` has been seen lagging behind a fast reconnect.
    fn poll_uninvited(&mut self, transport: &mut Transport, connected: &HashSet<Address>) {
        if !self.single() || self.pairing.is_some() {
            return;
        }
        let now = Instant::now();
        self.evicted.retain(|_, t| now.duration_since(*t) < EVICT_COOLDOWN);
        let uninvited: Vec<(u8, Address)> = self
            .bindings
            .iter()
            .filter(|(slot, b)| {
                **slot != self.active_slot
                    && (connected.contains(&b.addr) || transport.is_peer_connected(b.addr))
                    && !self.evicted.contains_key(&b.addr)
            })
            .map(|(&slot, b)| (slot, b.addr))
            .collect();
        for (slot, addr) in uninvited {
            info!(peer = %addr, slot, active = self.active_slot, "bound host connected while its slot is not active — dropping");
            transport.drop_peer(addr);
            self.evicted.insert(addr, now);
        }
    }

    /// Give up on held frames once the reconnect has clearly not happened
    /// (the host is off, or out of range). Loud only if there was input.
    fn poll_pending(&mut self) {
        if self.pending.as_ref().is_some_and(|p| p.since.elapsed() >= REPLAY_WINDOW) {
            let p = self.pending.take().expect("checked above");
            let (dropped_motion, evicted) = p.buffer.stats();
            if p.buffer.is_empty() {
                debug!(peer = %p.addr, "host has not come back yet; no longer holding frames for it");
            } else {
                warn!(peer = %p.addr, held = p.buffer.len(), dropped_motion, evicted, "host did not come back after the switch; held frames dropped");
            }
        }
    }

    /// Once a second: a bound host that bluetoothd shows connected, yet has
    /// had no input-report session for `STUCK_HOST_GRACE`, is either a host
    /// whose notification re-enable bluetoothd swallowed (its CCC state
    /// survived the disconnect — stock bluetoothd; the patched one on the Pi
    /// forgets it) or a phone that subscribes in its own time. Remedies,
    /// cheapest first:
    ///
    /// 1. Disconnect just that host. It reconnects within a second and, in
    ///    practice, gets a fresh AcquireNotify; nobody else notices.
    /// 2. If it comes back stuck again within `KICK_MEMORY` and *no other
    ///    host is subscribed*, re-register the GATT application — the only
    ///    thing that clears stock bluetoothd's stored CCC state. It costs
    ///    every connected host a Service Changed round trip, and macOS does
    ///    not re-attach its HID driver afterwards (it re-subscribes and then
    ///    ignores the reports until reconnected), so with other hosts live it
    ///    is not worth it: the host is left alone for this connection.
    async fn poll_stuck_hosts(&mut self, transport: &mut Transport, connected: &HashSet<Address>) {
        if self.kind != TransportKind::Le {
            return;
        }
        let now = Instant::now();
        self.kicked.retain(|_, t| now.duration_since(*t) < KICK_MEMORY);
        let mut trigger = None;
        let mut others_subscribed = false;
        for (&slot, b) in &self.bindings {
            let addr = b.addr;
            // Single mode: a host we are disconnecting or evicting is not
            // stuck, it is leaving.
            if !self.serves(slot) {
                continue;
            }
            if !connected.contains(&addr) {
                self.stuck.remove(&addr);
                self.helped.remove(&addr);
                continue;
            }
            if transport.is_peer_connected(addr) {
                self.stuck.remove(&addr);
                self.kicked.remove(&addr);
                others_subscribed = true;
                continue;
            }
            if self.helped.contains(&addr) {
                continue;
            }
            let since = *self.stuck.entry(addr).or_insert(now);
            if now.duration_since(since) >= STUCK_HOST_GRACE && trigger.is_none() {
                trigger = Some((addr, b.name.clone()));
            }
        }
        let Some((addr, name)) = trigger else { return };
        if let std::collections::hash_map::Entry::Vacant(e) = self.kicked.entry(addr) {
            warn!(
                %addr, %name,
                "host connected but not subscribed to input reports; disconnecting it so it \
                 reconnects afresh"
            );
            transport.drop_peer(addr);
            e.insert(now);
            self.stuck.remove(&addr);
            return;
        }
        if others_subscribed {
            warn!(
                %addr, %name,
                "host still not subscribed after a reconnect; leaving it alone rather than \
                 re-registering the HID application under the other hosts"
            );
            self.helped.insert(addr);
            self.stuck.remove(&addr);
            return;
        }
        if self
            .last_reregister
            .is_some_and(|t| now.duration_since(t) < REREGISTER_COOLDOWN)
        {
            return;
        }
        warn!(
            %addr, %name,
            "host still not subscribed after a reconnect; re-registering the HID application"
        );
        match transport.reregister_le().await {
            Ok(()) => {
                self.last_reregister = Some(now);
                self.stuck.clear();
                self.kicked.clear();
                // Every host with a live link gets the same reset; count them
                // all as served so a host that ignores Service Changed does
                // not keep us re-registering.
                for b in self.bindings.values() {
                    self.helped.insert(b.addr);
                }
            }
            Err(err) => warn!(err = format!("{err:#}"), "re-registering the HID application failed"),
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
        if let Some(slot) = self.slot_of(peer) {
            // A bound host. Single mode serves only the active slot's; one
            // that shows up anyway (it woke and saw us advertising for
            // another host) is sent away — except during a pairing window,
            // when it is most likely the host that was just bound to another
            // slot and is still finishing its bond.
            if !self.serves(slot) && self.pairing.is_none() {
                info!(%peer, slot, active = self.active_slot, "bound host connected while its slot is not active — dropping");
                transport.drop_peer(peer);
                self.evicted.insert(peer, Instant::now());
            }
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
        state::save(&self.state_path, &self.bindings, self.active_slot);
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
/// Whether to run the connectable advertisement: it is what lets a bonded
/// host reconnect, and it costs radio time while it runs. With every host
/// kept connected, advertise while any bound one is away; with one host at a
/// time, only while the active slot's host is away (never for the local slot
/// or an unbound one — nobody is expected). `connected` is bluetoothd's view
/// (`Device1.Connected`): the link is what matters here, the input-report
/// subscription that follows is the stuck-host logic's business.
fn wants_advertising(mode: Connections, active_host: Option<Address>, bound: &[Address], connected: &HashSet<Address>) -> bool {
    match mode {
        Connections::All => bound.iter().any(|a| !connected.contains(a)),
        Connections::Single => active_host.is_some_and(|a| !connected.contains(&a)),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8) -> Address {
        Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, last])
    }

    #[test]
    fn all_mode_advertises_while_any_bound_host_is_away() {
        let bound = [addr(1), addr(2)];
        let here: HashSet<Address> = [addr(1)].into_iter().collect();
        assert!(wants_advertising(Connections::All, Some(addr(1)), &bound, &here));
        let both: HashSet<Address> = bound.iter().copied().collect();
        assert!(!wants_advertising(Connections::All, Some(addr(1)), &bound, &both));
        // Bound hosts away, local slot active: still advertise — they may come back.
        assert!(wants_advertising(Connections::All, None, &bound, &here));
    }

    #[test]
    fn single_mode_advertises_only_for_the_active_host() {
        let bound = [addr(1), addr(2)];
        let none = HashSet::new();
        assert!(wants_advertising(Connections::Single, Some(addr(2)), &bound, &none));
        let two: HashSet<Address> = [addr(2)].into_iter().collect();
        assert!(!wants_advertising(Connections::Single, Some(addr(2)), &bound, &two));
        // The other host being away is not our concern.
        let one: HashSet<Address> = [addr(1)].into_iter().collect();
        assert!(wants_advertising(Connections::Single, Some(addr(2)), &bound, &one));
    }

    #[test]
    fn single_mode_never_advertises_for_the_local_or_an_unbound_slot() {
        let bound = [addr(1), addr(2)];
        let none = HashSet::new();
        assert!(!wants_advertising(Connections::Single, None, &bound, &none));
    }
}
