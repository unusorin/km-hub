//! HID transport over Bluetooth L2CAP, plus a logging stub used by default
//! until the real link is proven.
//!
//! The control channel gets its own task per connection that answers HIDP
//! requests (SET_PROTOCOL, GET_REPORT, ...) — hosts like macOS reset the
//! link within seconds if these go unanswered.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bluer::l2cap::{Security, SecurityLevel, Socket, SocketAddr, Stream, StreamListener};
use bluer::{Address, AddressType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::gatt::LeTransport;
use crate::hid::{BATTERY_LEVEL, BATTERY_REPORT_ID, HIDP_DATA_INPUT, HidFrame};

pub const PSM_HID_CONTROL: u16 = 0x0011;
pub const PSM_HID_INTERRUPT: u16 = 0x0013;

/// HID links must be authenticated and encrypted (BT HID spec §5.4.3.4).
/// Requiring it at the socket level makes the kernel run SSP pairing *before*
/// the channel is accepted, so a host connecting during a pairing window ends
/// up bonded (link key stored) rather than "paired, no bonding" — hosts such
/// as BlueZ (`ClassicBondedOnly`) refuse HID from non-bonded devices.
const HID_SECURITY: Security = Security { level: SecurityLevel::Medium, key_size: 0 };

/// A stalled link must fail fast instead of blocking the report stream.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Let the ACL link enter sniff mode.
///
/// bluer's L2CAP sockets default to `BT_POWER force_active = ON`, which makes
/// the kernel emit an EXIT_SNIFF_MODE before every outgoing packet — at 125 Hz
/// that pins the link in active mode permanently, whatever the adapter's link
/// policy says.
///
/// Active mode looks like the low-latency choice and is the opposite under
/// load. Commercial HID devices (Magic Mouse, MX Keys) run sniff + SSR, which
/// reserves anchor points in the *host's* baseband schedule; an active link is
/// best-effort, and best-effort is the first thing starved when the host's
/// radio gets busy — which is exactly what opening its Bluetooth settings pane
/// does. Sniff is a slot reservation, not a demotion.
///
/// Two system-side knobs must agree or this has no effect (see `setup.sh`):
/// the adapter's link policy has to include sniff, and `idle_timeout` has to be
/// non-zero or the kernel never schedules the transition.
fn allow_sniff(stream: &Stream, peer: Address, channel: &str) {
    if let Err(err) = stream.as_ref().set_power_forced_active(false) {
        debug!(%peer, channel, %err, "cannot clear BT_POWER force_active");
    }
}

pub enum Transport {
    Log(LogTransport),
    L2cap(L2capTransport),
    /// HID over GATT — Bluetooth LE peripheral (see `bt::gatt`).
    Le(LeTransport),
}

impl Transport {
    /// Send a frame to `peer` (no-op with a debug log if it is not connected).
    pub async fn send_to(&mut self, peer: Address, frame: &HidFrame) -> Result<()> {
        match self {
            Transport::Log(t) => t.send(peer, frame),
            Transport::L2cap(t) => t.send_to(peer, frame).await,
            Transport::Le(t) => t.send_to(peer, frame).await,
        }
    }

    pub fn is_peer_connected(&self, peer: Address) -> bool {
        match self {
            Transport::Log(_) => true,
            Transport::L2cap(t) => t.conns.contains_key(&peer),
            Transport::Le(t) => t.is_peer_connected(peer),
        }
    }

    pub fn has_listeners(&self) -> bool {
        match self {
            Transport::Log(_) => false,
            Transport::L2cap(t) => t.control_listener.is_some(),
            Transport::Le(t) => t.has_listeners(),
        }
    }

    /// Hosts dial *us* on LE; only the classic transport can place a call.
    pub fn can_dial(&self) -> bool {
        !matches!(self, Transport::Le(_))
    }

    /// Register a connection produced by [`L2capTransport::dial`].
    pub fn add_outgoing(&mut self, addr: Address, (control, interrupt): (Stream, Stream)) {
        if let Transport::L2cap(t) = self {
            info!(%addr, "HID L2CAP channels connected (outgoing)");
            t.insert(control, interrupt, addr);
        }
    }

    /// Close the connection to `peer` (if any).
    pub fn drop_peer(&mut self, peer: Address) {
        match self {
            Transport::Log(_) => {}
            Transport::L2cap(t) => {
                if t.conns.remove(&peer).is_some() {
                    info!(%peer, "HID connection closed");
                }
            }
            Transport::Le(t) => t.drop_peer(peer),
        }
    }

    /// Wait for an incoming HID connection (control then interrupt) and
    /// return the peer's address. Cancel-safe: an accepted control channel is
    /// parked until its interrupt channel arrives on a later call.
    /// Only meaningful for the L2CAP variant; guarded by `has_listeners()`.
    pub async fn accept_incoming(&mut self) -> Result<Address> {
        match self {
            Transport::Log(_) => futures::future::pending().await,
            Transport::L2cap(t) => t.accept_incoming().await,
            Transport::Le(t) => t.accept_incoming().await,
        }
    }
}

/// Logs every frame instead of sending it — the default transport so the
/// prototype always runs end-to-end without a working BT link.
pub struct LogTransport;

impl LogTransport {
    fn send(&mut self, peer: Address, frame: &HidFrame) -> Result<()> {
        info!(%peer, ?frame, "would send HID frame");
        Ok(())
    }
}

/// One live HID connection: the interrupt stream we write reports to, plus
/// the task answering HIDP requests on the control channel.
struct Conn {
    control_task: JoinHandle<()>,
    interrupt: Stream,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.control_task.abort();
    }
}

/// Holds one connection per host so switching targets is instant and idle
/// hosts stay attached; frames are routed by peer address.
pub struct L2capTransport {
    control_listener: Option<StreamListener>,
    interrupt_listener: Option<StreamListener>,
    /// Control channel accepted, interrupt channel not yet (see `accept_incoming`).
    pending_control: Option<(Stream, Address)>,
    conns: HashMap<Address, Conn>,
}

impl L2capTransport {
    /// Bind the HID PSMs on the local adapter. Fails with remediation advice
    /// when BlueZ's input plugin still owns them or the PSMs are privileged.
    pub async fn bind(adapter_addr: Address) -> Result<Self> {
        let bind_psm = |psm| async move {
            let sa = SocketAddr::new(adapter_addr, AddressType::BrEdr, psm);
            (|| -> std::io::Result<StreamListener> {
                let sock = Socket::<Stream>::new_stream()?;
                sock.set_security(HID_SECURITY)?;
                sock.bind(sa)?;
                sock.listen(1)
            })()
            .with_context(|| {
                format!(
                    "cannot bind L2CAP PSM {psm:#06x} — either BlueZ's input plugin owns it \
                     (run ./setup.sh to add '-P input' to bluetoothd) or the PSM is privileged \
                     (setcap cap_net_bind_service+ep on the binary, or run with sudo)"
                )
            })
        };
        let control_listener = bind_psm(PSM_HID_CONTROL).await?;
        let interrupt_listener = bind_psm(PSM_HID_INTERRUPT).await?;
        info!("L2CAP listeners bound on PSM 0x11/0x13");
        Ok(Self {
            control_listener: Some(control_listener),
            interrupt_listener: Some(interrupt_listener),
            pending_control: None,
            conns: HashMap::new(),
        })
    }

    fn insert(&mut self, control: Stream, interrupt: Stream, peer: Address) {
        if self.conns.remove(&peer).is_some() {
            debug!(%peer, "replacing existing connection from the same host");
        }
        allow_sniff(&control, peer, "control");
        allow_sniff(&interrupt, peer, "interrupt");
        self.conns.insert(peer, Conn { control_task: spawn_control_task(control, peer), interrupt });
    }

    /// Device-initiated (re)connection: control channel first, then interrupt,
    /// per the BT HID spec. Doesn't touch the transport so it can run
    /// concurrently with `accept_incoming`; hand the result to
    /// [`Transport::add_outgoing`].
    pub async fn dial(addr: Address) -> Result<(Stream, Stream)> {
        let dial = |psm| async move {
            let sock = Socket::<Stream>::new_stream()?;
            sock.set_security(HID_SECURITY)?;
            sock.connect(SocketAddr::new(addr, AddressType::BrEdr, psm)).await
        };
        let control = dial(PSM_HID_CONTROL)
            .await
            .context("control channel connect failed")?;
        let interrupt = dial(PSM_HID_INTERRUPT)
            .await
            .context("interrupt channel connect failed")?;
        Ok((control, interrupt))
    }

    async fn accept_incoming(&mut self) -> Result<Address> {
        let (Some(cl), Some(il)) = (&self.control_listener, &self.interrupt_listener) else {
            bail!("listeners not bound");
        };
        if self.pending_control.is_none() {
            let (control, peer) = cl.accept().await.context("control accept failed")?;
            debug!(peer = %peer.addr, "control channel accepted, waiting for interrupt channel");
            self.pending_control = Some((control, peer.addr));
        }
        let (interrupt, ipeer) = il.accept().await.context("interrupt accept failed")?;
        let (control, peer) = self.pending_control.take().expect("pending control set above");
        if ipeer.addr != peer {
            warn!(control = %peer, interrupt = %ipeer.addr, "interrupt channel from a different host — dropping both");
            bail!("mismatched HID channel peers");
        }
        info!(%peer, "HID L2CAP channels connected (incoming)");
        self.insert(control, interrupt, peer);
        Ok(peer)
    }

    async fn send_to(&mut self, peer: Address, frame: &HidFrame) -> Result<()> {
        let Some(conn) = self.conns.get_mut(&peer) else {
            debug!(%peer, "not connected, dropping frame");
            return Ok(());
        };
        let mut buf = Vec::with_capacity(1 + frame.bytes().len());
        buf.push(HIDP_DATA_INPUT);
        buf.extend_from_slice(frame.bytes());
        let failed = match tokio::time::timeout(WRITE_TIMEOUT, conn.interrupt.write_all(&buf)).await {
            Ok(Ok(())) => false,
            Ok(Err(err)) => {
                warn!(%peer, %err, "interrupt channel write failed, dropping connection");
                true
            }
            Err(_) => {
                warn!(%peer, "interrupt channel write timed out, dropping connection");
                true
            }
        };
        if failed {
            self.conns.remove(&peer);
        }
        Ok(())
    }
}

// HIDP message types (header high nibble).
const HIDP_HANDSHAKE: u8 = 0x0;
const HIDP_HID_CONTROL: u8 = 0x1;
const HIDP_GET_REPORT: u8 = 0x4;
const HIDP_SET_REPORT: u8 = 0x5;
const HIDP_GET_PROTOCOL: u8 = 0x6;
const HIDP_SET_PROTOCOL: u8 = 0x7;
const HIDP_DATA: u8 = 0xA;

const HANDSHAKE_SUCCESS: u8 = 0x00;
const HANDSHAKE_ERR_UNSUPPORTED: u8 = 0x03;

const HID_CONTROL_VIRTUAL_CABLE_UNPLUG: u8 = 0x05;

const REPORT_TYPE_INPUT: u8 = 1;
const REPORT_TYPE_FEATURE: u8 = 3;

/// Answer HIDP requests on the control channel for the lifetime of one
/// connection. Hosts (macOS in particular) reset the whole HID link if these
/// requests go unanswered.
fn spawn_control_task(control: Stream, peer: Address) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut rd, mut wr) = tokio::io::split(control);
        // 0 = boot, 1 = report. We always stream report-protocol frames; boot
        // mode is acknowledged but not honored (logged for visibility).
        let mut protocol: u8 = 1;
        let mut buf = [0u8; 128];
        loop {
            let n = match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let (msg, param) = (buf[0] >> 4, buf[0] & 0x0F);
            debug!(%peer, msg, param, len = n, "control channel request");
            let reply: Option<Vec<u8>> = match msg {
                // Header, then the report ID, then (if the SIZE flag is set) a
                // 2-byte buffer size we don't need — the ID stays at buf[1]
                // either way.
                HIDP_GET_REPORT => match (param & 0x03, buf.get(1).copied()) {
                    (REPORT_TYPE_INPUT, id) => {
                        // Reply with the zero state of the requested report.
                        let report: &[u8] = match id {
                            Some(2) => &[2, 0, 0, 0, 0],
                            _ => &[1, 0, 0, 0, 0, 0, 0, 0, 0],
                        };
                        let mut r = vec![(HIDP_DATA << 4) | REPORT_TYPE_INPUT];
                        r.extend_from_slice(report);
                        Some(r)
                    }
                    // The battery level macOS polls when its Bluetooth pane is
                    // open. `None` covers a host that asks without naming an ID.
                    (REPORT_TYPE_FEATURE, Some(BATTERY_REPORT_ID) | None) => {
                        debug!(%peer, "battery feature report requested");
                        Some(vec![
                            (HIDP_DATA << 4) | REPORT_TYPE_FEATURE,
                            BATTERY_REPORT_ID,
                            BATTERY_LEVEL,
                        ])
                    }
                    _ => Some(vec![HANDSHAKE_ERR_UNSUPPORTED]),
                },
                HIDP_SET_REPORT => Some(vec![HANDSHAKE_SUCCESS]),
                HIDP_GET_PROTOCOL => Some(vec![HIDP_DATA << 4, protocol]),
                HIDP_SET_PROTOCOL => {
                    protocol = param & 0x01;
                    if protocol == 0 {
                        info!(%peer, "host requested boot protocol — acknowledged but streaming report protocol");
                    }
                    Some(vec![HANDSHAKE_SUCCESS])
                }
                HIDP_HID_CONTROL => {
                    if param == HID_CONTROL_VIRTUAL_CABLE_UNPLUG {
                        info!(%peer, "host sent VIRTUAL_CABLE_UNPLUG");
                        break;
                    }
                    None // SUSPEND / EXIT_SUSPEND / resets: no response required
                }
                // Output reports from the host (e.g. keyboard LED state).
                HIDP_DATA => None,
                HIDP_HANDSHAKE => None,
                _ => Some(vec![HANDSHAKE_ERR_UNSUPPORTED]),
            };
            if let Some(reply) = reply
                && wr.write_all(&reply).await.is_err()
            {
                break;
            }
        }
        debug!(%peer, "control channel task ended");
    })
}
