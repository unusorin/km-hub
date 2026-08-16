//! The router owns the switching state: it feeds every event through the
//! hotkey FSM, then dispatches to the local uinput sink or the HID translator
//! + Bluetooth channel depending on the active slot.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use evdev::{EventSummary, InputEvent, KeyCode};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::bt::BtCmd;
use crate::config::LOCAL_SLOT;
use crate::hid::HidFrame;
use crate::hid::translate::Translator;
use crate::hooks::HookEvent;
use crate::input::LocalSink;
use crate::input::hotkey::{HotkeyFsm, Verdict};
use crate::input::keys::KeyCombo;
use crate::rgb::{Activity, RgbEvent};
use crate::state::Bindings;

pub enum RouterMsg {
    Input(InputEvent),
}

pub struct Router {
    slot: u8,
    fsm: HotkeyFsm,
    translator: Translator,
    sink: LocalSink,
    bt_tx: mpsc::Sender<(u8, HidFrame)>,
    bt_cmd_tx: mpsc::Sender<BtCmd>,
    slot_tx: watch::Sender<u8>,
    /// Slot activations and macro presses for the hook task. `None` when
    /// neither hooks nor macros are configured.
    hook_tx: Option<mpsc::Sender<HookEvent>>,
    /// Keeps the lighting awake while the user is at the desk. `None` when
    /// lighting is off.
    activity: Option<Activity>,
    /// Repaint requests for the lighting task. `None` when lighting is off.
    rgb_tx: Option<mpsc::Sender<RgbEvent>>,
    bindings_rx: watch::Receiver<Bindings>,
    /// Keys forwarded as pressed to the *local* target; used to synthesize
    /// releases when switching away. (The translator tracks the remote side.)
    local_held: HashSet<KeyCode>,
    dropped_frames: u64,
    /// Mouse motion pacing: minimum spacing between motion reports.
    mouse_interval: Duration,
    mouse_last_sent: Instant,
    /// Deadline of a deferred motion flush, if motion arrived too soon after
    /// the previous report.
    mouse_flush_at: Option<Instant>,
    /// A combo F-key being held: once the deadline passes, the slot is asked to
    /// re-pair (see `REPAIR_HOLD`).
    repair_hold: Option<RepairHold>,
}

/// How long a slot's combo must be held to force it to re-pair — Shift makes
/// no difference, it is the held F-key that means this. Long enough that
/// leaning on the combo cannot cost you a pairing.
const REPAIR_HOLD: Duration = Duration::from_secs(3);

struct RepairHold {
    slot: u8,
    at: Instant,
    /// Already requested; do not fire again while the key stays down.
    fired: bool,
}

/// What the router needs to run, in the same shape as `BtDeps`/`RgbDeps`.
pub struct RouterDeps {
    pub sink: LocalSink,
    pub bt_tx: mpsc::Sender<(u8, HidFrame)>,
    pub bt_cmd_tx: mpsc::Sender<BtCmd>,
    pub slot_tx: watch::Sender<u8>,
    pub hook_tx: Option<mpsc::Sender<HookEvent>>,
    /// Macro combos in config order; the FSM reports one by its index.
    pub macros: Vec<KeyCombo>,
    pub activity: Option<Activity>,
    pub rgb_tx: Option<mpsc::Sender<RgbEvent>>,
    pub bindings_rx: watch::Receiver<Bindings>,
    pub mouse_rate_hz: u32,
}

impl Router {
    pub fn new(deps: RouterDeps) -> Self {
        let RouterDeps {
            sink,
            bt_tx,
            bt_cmd_tx,
            slot_tx,
            hook_tx,
            macros,
            activity,
            rgb_tx,
            bindings_rx,
            mouse_rate_hz,
        } = deps;
        let mouse_interval = Duration::from_micros(1_000_000 / u64::from(mouse_rate_hz.max(1)));
        Self {
            slot: LOCAL_SLOT,
            fsm: HotkeyFsm::with_macros(macros),
            translator: Translator::new(),
            sink,
            bt_tx,
            bt_cmd_tx,
            slot_tx,
            hook_tx,
            activity,
            rgb_tx,
            bindings_rx,
            local_held: HashSet::new(),
            dropped_frames: 0,
            mouse_interval,
            mouse_last_sent: Instant::now() - mouse_interval,
            mouse_flush_at: None,
            repair_hold: None,
        }
    }

    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<RouterMsg>,
        cancel: CancellationToken,
    ) -> Result<()> {
        info!(
            slot = self.slot,
            mouse_rate_hz = 1_000_000 / self.mouse_interval.as_micros().max(1),
            "router started (local target active)"
        );
        loop {
            let flush_at = self.mouse_flush_at.unwrap_or_else(Instant::now);
            let repair_at = self
                .repair_hold
                .as_ref()
                .filter(|hold| !hold.fired)
                .map(|hold| hold.at);
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = rx.recv() => match msg {
                    Some(RouterMsg::Input(event)) => {
                        if let Err(err) = self.handle(event) {
                            warn!(%err, "event handling failed");
                        }
                        self.sync_repair_hold();
                    }
                    None => break,
                },
                _ = tokio::time::sleep_until(flush_at), if self.mouse_flush_at.is_some() => {
                    self.flush_mouse_now();
                }
                _ = tokio::time::sleep_until(repair_at.unwrap_or_else(Instant::now)),
                    if repair_at.is_some() => {
                    self.fire_repair();
                }
            }
        }
        Ok(())
    }

    /// Pace mouse motion: send immediately if the link has been quiet for an
    /// interval (no added latency for the first movement), otherwise keep
    /// accumulating and flush when the interval elapses. Button changes and
    /// keyboard reports always go out immediately.
    fn flush_remote(&mut self) {
        for frame in self.translator.flush_keyboard() {
            self.send_frame(frame);
        }
        for frame in self.translator.flush_consumer() {
            self.send_frame(frame);
        }
        if self.translator.mouse_buttons_dirty()
            || Instant::now() >= self.mouse_last_sent + self.mouse_interval
        {
            self.flush_mouse_now();
        } else if self.translator.mouse_motion_pending() && self.mouse_flush_at.is_none() {
            self.mouse_flush_at = Some(self.mouse_last_sent + self.mouse_interval);
        }
    }

    fn flush_mouse_now(&mut self) {
        self.mouse_flush_at = None;
        let frames = self.translator.flush_mouse();
        if frames.is_empty() {
            return;
        }
        self.mouse_last_sent = Instant::now();
        for frame in frames {
            self.send_frame(frame);
        }
    }

    fn handle(&mut self, event: InputEvent) -> Result<()> {
        // Every event counts, including the combos the FSM swallows below: the
        // user pressing Ctrl+Alt+F3 is as present as one typing a letter.
        if let Some(activity) = &mut self.activity {
            activity.ping(Instant::now());
        }
        match event.destructure() {
            EventSummary::Key(_, key, value) => match self.fsm.on_key(key, value) {
                Verdict::Swallow => {}
                Verdict::SwitchTo { slot, fire_hooks } => self.switch_to(slot, fire_hooks)?,
                Verdict::RunMacro { index, held_mods } => self.run_macro(index, &held_mods)?,
                Verdict::Pass => {
                    if self.slot == LOCAL_SLOT {
                        match value {
                            1 => {
                                self.local_held.insert(key);
                            }
                            0 => {
                                self.local_held.remove(&key);
                            }
                            _ => {}
                        }
                        self.sink.emit(&event)?;
                    } else {
                        self.translator.handle_key(key, value);
                    }
                }
            },
            EventSummary::RelativeAxis(_, axis, value) => {
                if self.slot == LOCAL_SLOT {
                    self.sink.emit(&event)?;
                } else {
                    self.translator.handle_rel(axis, value);
                }
            }
            // Local side needs no SYN handling: VirtualDevice::emit appends its own.
            EventSummary::Synchronization(..) if self.slot != LOCAL_SLOT => self.flush_remote(),
            _ => {}
        }
        Ok(())
    }

    /// Arm (or disarm) the re-pair timer from the physically held combo key.
    /// The local slot is skipped: there is nothing to pair with the hub itself.
    fn sync_repair_hold(&mut self) {
        match self.fsm.combo_slot_held() {
            Some(slot) if slot != LOCAL_SLOT => {
                if self.repair_hold.as_ref().map(|hold| hold.slot) != Some(slot) {
                    self.repair_hold = Some(RepairHold {
                        slot,
                        at: Instant::now() + REPAIR_HOLD,
                        fired: false,
                    });
                }
            }
            _ => self.repair_hold = None,
        }
    }

    /// The combo has been held long enough: ask the Bluetooth side to drop the
    /// slot's binding and pairing and reopen a pairing window.
    fn fire_repair(&mut self) {
        let Some(hold) = self.repair_hold.as_mut() else { return };
        hold.fired = true;
        let slot = hold.slot;
        info!(
            slot,
            held_secs = REPAIR_HOLD.as_secs(),
            "combo held — requesting re-pair for this slot"
        );
        if self.bt_cmd_tx.try_send(BtCmd::RepairSlot { slot }).is_err() {
            warn!(slot, "bluetooth task not accepting commands");
        }
    }

    /// `fire_hooks` is the Shift in Ctrl+Alt+Shift+F<n>. Switching is otherwise
    /// silent: moving the keyboard to a machine and triggering the side effect
    /// that belongs to it are separate wishes, and only the second one is worth
    /// asking for explicitly.
    fn switch_to(&mut self, slot: u8, fire_hooks: bool) -> Result<()> {
        // A combo pressed for the slot you are already on is a re-assert, not a
        // switch: nothing about the routing changes, but everything that hangs
        // off the slot is asserted again. That is the retry for a side effect
        // that didn't take the first time — an IR command the HDMI switch
        // missed, or a device that took half a color update and left the
        // keyboard two-tone.
        if slot == self.slot {
            debug!(slot, hooks = fire_hooks, "already on this slot — re-asserting");
            // Lighting always: it is the half of the re-assert with no side
            // effects, so it needs no opt-in. Hooks only with Shift, as ever.
            self.repaint_lighting();
            if fire_hooks {
                self.notify_hooks(slot);
            }
            return Ok(());
        }
        let name = if slot == LOCAL_SLOT {
            "local".to_string()
        } else {
            match self.bindings_rx.borrow().get(&slot) {
                Some(binding) => binding.name.clone(),
                None => {
                    // Unbound slot: open a pairing window instead of switching.
                    // The combo was already swallowed by the FSM either way.
                    // No hooks even with Shift — the slot never became active.
                    info!(slot, "slot unbound — requesting pairing window");
                    if self.bt_cmd_tx.try_send(BtCmd::OpenPairingWindow { slot }).is_err() {
                        warn!(slot, "bluetooth task not accepting commands");
                    }
                    return Ok(());
                }
            }
        };

        // Release everything on the target we are leaving.
        if self.slot == LOCAL_SLOT {
            let held: Vec<KeyCode> = self.local_held.drain().collect();
            if !held.is_empty() {
                self.sink.release_keys(&held)?;
            }
        } else {
            self.mouse_flush_at = None;
            for frame in self.translator.release_all() {
                self.send_frame(frame);
            }
        }

        info!(from = self.slot, to = slot, target = name, hooks = fire_hooks, "switching target");
        self.slot = slot;
        let _ = self.slot_tx.send(slot);
        if fire_hooks {
            self.notify_hooks(slot);
        }
        Ok(())
    }

    /// Hooks get their own channel rather than sharing `slot_tx`: a `watch`
    /// coalesces, so a fast 2→3→2 could drop an activation entirely. Missing
    /// one is harmless for lighting and a bug for a side effect. Never blocks —
    /// a slow hook task loses the event instead of stalling input.
    /// Ask the lighting to push itself to the hardware again. Cosmetic and
    /// best-effort like everything else on this path: a full channel means the
    /// task already has work queued, which repaints anyway.
    fn repaint_lighting(&self) {
        if let Some(tx) = &self.rgb_tx
            && tx.try_send(RgbEvent::Repaint).is_err()
        {
            debug!("lighting task not accepting events — skipping the repaint");
        }
    }

    fn notify_hooks(&self, slot: u8) {
        if let Some(tx) = &self.hook_tx
            && tx.try_send(HookEvent::Slot(slot)).is_err()
        {
            warn!(slot, "hook task not accepting events — skipping this activation");
        }
    }

    /// A macro combo was pressed: same fire-and-forget hand-off as a hook, on
    /// whatever slot is active. The combo itself was swallowed by the FSM —
    /// but its modifiers were forwarded as pressed *before* the trigger made
    /// it a combo, and their releases will be swallowed too. A switch releases
    /// everything on the target it leaves; a macro leaves nothing, so release
    /// those modifiers here or the target keeps a phantom Ctrl held down.
    fn run_macro(&mut self, index: usize, held_mods: &[KeyCode]) -> Result<()> {
        debug!(index, slot = self.slot, ?held_mods, "macro combo pressed");
        if self.slot == LOCAL_SLOT {
            let held: Vec<KeyCode> = held_mods
                .iter()
                .copied()
                .filter(|k| self.local_held.remove(k))
                .collect();
            if !held.is_empty() {
                self.sink.release_keys(&held)?;
            }
        } else {
            for key in held_mods {
                self.translator.handle_key(*key, 0);
            }
            self.flush_remote();
        }
        if let Some(tx) = &self.hook_tx
            && tx.try_send(HookEvent::Macro(index)).is_err()
        {
            warn!(index, "hook task not accepting events — skipping this macro");
        }
        Ok(())
    }

    /// Never block on the Bluetooth side: a stalled link must not
    /// back-pressure input handling.
    fn send_frame(&mut self, frame: HidFrame) {
        // Tagged with the *current* slot: releases emitted while switching
        // away must reach the host we are leaving, not the new one.
        if self.bt_tx.try_send((self.slot, frame)).is_err() {
            self.dropped_frames += 1;
            if self.dropped_frames % 100 == 1 {
                warn!(dropped = self.dropped_frames, "BT channel full, dropping HID frames");
            }
        }
    }
}
