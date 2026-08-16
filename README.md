# km-hub

**A Raspberry Pi keyboard/mouse hub that switches your wired input between computers over Bluetooth.**

Plug a USB keyboard and mouse into the Pi. km-hub presents them as a single Bluetooth HID
device to every paired computer (Linux, macOS, …) and forwards input to whichever one you pick
with `Ctrl+Alt+F<n>`. Nothing to install on the clients — pair once, then switch with a hotkey.

- Slot 1 (`Ctrl+Alt+F1`) — local passthrough on the hub itself (planned: USB gadget passthrough).
- Slots 2–12 (`Ctrl+Alt+F2` … `F12`) — Bluetooth clients. Pressing the combo for an *unbound*
  slot opens a 60 s pairing window; pair from the new computer and the slot is remembered.
- **Holding** a slot's combo for 3 s forces it to re-pair: the binding is forgotten, the pairing is
  removed from BlueZ, and a fresh window opens. Use it when a client has gone stale, or to hand the
  slot to a different machine.
- Adding **Shift** (`Ctrl+Alt+Shift+F<n>`) switches *and* runs that slot's [hooks](#hooks-optional).
  Plain switching is silent, so you can look at another machine without also throwing the monitor over.
- All paired hosts stay connected; switching is instant. Keys held during a switch are released
  on the host you leave.

## Hardware

- Raspberry Pi 4B (any RAM size; the built-in Bluetooth is used) running Raspberry Pi OS /
  Debian 12+ (BlueZ ≥ 5.66).
- USB keyboard and mouse plugged into the Pi.
- Clients with Bluetooth. Clients must **pair** (not just connect) — a plain "Connect" leaves
  the link unbonded and BlueZ hosts refuse HID input from unbonded devices.

## Dependencies

On the Pi: only `bluez` (`sudo apt install bluez`). No toolchain — the binary is cross-compiled on
the machine you develop from.

On the development machine (x86_64 Linux):

```sh
curl https://sh.rustup.rs -sSf | sh            # Rust toolchain (edition 2024, rustc ≥ 1.85)
rustup target add aarch64-unknown-linux-gnu    # Pi target
sudo apt install gcc-aarch64-linux-gnu         # cross linker (wired up in .cargo/config.toml)
```

plus `rsync`, `ssh` (key-based login to the Pi) and `make`. Rust crates (pulled by Cargo): `bluer`
(BlueZ D-Bus + L2CAP), `evdev`, `tokio`, `serde`/`toml`, `tracing`, `dbus` — the last one with its
`vendored` feature, so libdbus is compiled into the binary and no aarch64 `libdbus-1-dev` sysroot
is needed on the host.

## Build & deploy

The Makefile cross-builds locally and uploads only the binary plus the support files
(`setup.sh`, `km-hub.service`, `config*.toml`) to `~/km-hub` on the Pi. Per-machine parameters
live in `.env` (gitignored):

```sh
cp .env.example .env    # then edit PI=user@host; also REMOTE_DIR, TARGET, TRANSPORT, RUST_LOG
make deploy             # cargo build --release --target aarch64 + rsync + setcap on the Pi
make setup              # one-time system setup on the Pi (interactive)
make run                # run in the foreground on the Pi (Ctrl-C to stop)
make ssh                # shell on the Pi in the project dir
```

Command-line variables still override `.env` (`make deploy PI=admin@other-host`). This local
`.env` is unrelated to `/home/admin/km-hub.env` on the Pi, which holds the `[[hook]]` secrets
read by the systemd unit.

`setup.sh` (run by `make setup`) is idempotent and prompts before each step:

1. udev rules + `input` group for `/dev/input/*` and `/dev/uinput`, loads the `uinput` module.
2. Unblocks Bluetooth if the image ships it rfkill soft-blocked.
3. Starts `bluetoothd` with `-P input,sap,avrcp,a2dp`: `input` frees the HID L2CAP PSMs
   (0x11/0x13) for km-hub, and the rest stop the hub advertising SIM Access, A/V Remote Control and
   audio profiles it does not implement (SAP also sets the Telephony bit in the class of device).
4. Sets the adapter class to keyboard/mouse combo (`0x0005C0`) so macOS lists it as an input device.
   Then a `km-hub-linktune` unit that sets, at every boot, the kernel's per-adapter link knobs:
   sniff-mode intervals for classic HID and the LE connection interval (11.25–15 ms), supervision
   timeout and advertising interval for `--transport le` — none of which BlueZ sets for us.
   Finally, optional and only for LE mode: `ControllerMode = le`, which turns off the classic side
   so a dual-mode host cannot pair the transport we don't serve (breaks `--transport l2cap`).
5. Grants `cap_net_bind_service` to the binary (redone by `make deploy`).
6. Optional: installs OpenRGB (upstream `.deb` + udev rules) and an `openrgb.service` running its
   SDK server on `127.0.0.1:6742`, for the slot lighting below. The service runs as root because
   OpenRGB's udev rules grant device access via `uaccess`, which only covers a seat-local login
   session — a hub reached over ssh has none.

`./setup.sh --undo` reverts steps 3–4 (plugins, class, link tuning, LE-only mode) — use it on a
machine that ran the setup but should now be a *client* (with `-P input`, bluetoothd cannot act as
an HID host), or to go back from LE-only to classic HID.

## Configure

Copy `config.example.toml` to `config.toml` (next to the binary's working directory) and adjust:

```toml
adapter_alias = "KMHub"                   # name shown to clients
# mouse_rate_hz = 125                     # cap for mouse *motion* reports over BT
# devices = ["/dev/input/by-id/..."]      # default: auto-detect keyboards/mice

[[target]]
name = "tower"     # pre-name a slot; it binds via the pairing window
slot = 2

[[target]]
name = "macbook"
slot = 3
# mac = "AA:BB:CC:DD:EE:FF"   # optional: pre-seed a slot with a known address
```

Learned bindings are stored in `state.toml` next to `config.toml`.

### Lighting (optional)

With an OpenRGB server on the hub (setup step 6), km-hub colors the hub's *own* keyboard and mouse
by active slot, so you can see where your typing is going without looking at a screen:

```toml
[rgb]
enabled = true
brightness = 60                # 0-100; devices without brightness control ignore it
local_color   = "#ffffff"      # slot 1 — the hub itself
pairing_color = "#ffa000"      # blinks at 1 Hz while a pairing window is open
bound_color   = "#00ff00"      # flashes twice when a device binds
idle_timeout_secs = 120        # fade out after 2 min with no input; 0 = never
palette = ["#ff0000", "#0000ff", "#00ff00"]   # slot 2, 3, 4, … (wraps)

[[target]]
name = "tower"
slot = 2
color = "#ff0000"              # optional per-slot override of the palette
# brightness = 80              # optional per-slot override
```

Every device OpenRGB detects is driven. OpenRGB only detects hardware when it starts, so after
plugging in a new RGB device run `sudo systemctl restart openrgb` — km-hub reconnects on its own
and picks it up. (If OpenRGB comes up with *nothing* to drive, e.g. it won the boot race against
USB enumeration, km-hub asks it to re-detect every 30 s until something appears.)

If a device ever takes half a color update and leaves the keyboard two-tone, press the slot's combo
again: `Ctrl+Alt+F<n>` for the slot you are *already* on re-reads the device list and repaints
everything from scratch, rather than deduplicating itself away as "nothing changed".

Two minutes without a keypress or a mouse movement and the LEDs fade out, so an empty desk is not
lit all night; the next input fades them back to the slot color. Only the lighting idles — km-hub
keeps routing, hooks keep firing and the Bluetooth links stay up regardless. Tune it with
`idle_timeout_secs`, or set it to 0 to keep the LEDs on permanently.

Without the `[rgb]` table lighting is off and nothing connects to OpenRGB. Lighting is cosmetic by
design: if the server is stopped or missing, km-hub warns once, retries in the background and keeps
switching exactly as before.

### Hooks (optional)

A hook runs a command when you activate a slot with `Ctrl+Alt+Shift+F<n>` — go to the macbook *and*
bring the monitor with you, come back to the hub and put it back. km-hub only spawns the command;
what it talks to is your business:

```toml
[[hook]]
slot = 2
name = "macbook on"            # optional, only used in log lines
run  = "curl -fsS -XPOST -H \"Authorization: Bearer $HA_TOKEN\" http://ha.local:8123/api/webhook/km-macbook"

[[hook]]
slot = 1                       # slot 1 (the hub itself) is hookable, unlike [[target]]
run  = ["/home/admin/bin/desk-local.sh"]
# timeout_secs = 30            # default 10, max 300
```

A **string** runs through `sh -c`, so `$VARS`, pipes and redirection work. An **array** is exec'd
directly with no shell, so nothing is expanded and nothing needs quoting. Pick per hook.

Hooks run **only** on `Ctrl+Alt+Shift+F<n>`, which also switches to slot n. Plain `Ctrl+Alt+F<n>`
switches silently and never runs anything — moving the keyboard to a machine and triggering the side
effect that belongs to it are separate wishes, and only the second one is worth asking for.
Pressing either combo for the slot you are already on is a re-assert: no switch happens, but
everything that hangs off the slot is asserted again — the retry for something that didn't take the
first time. The Shift combo runs the hooks again, for an IR command the HDMI switch missed; write
hooks so running one twice is harmless. Both combos repaint the lighting, for a device that took
half a color update and left the keyboard two-tone. A slot that is still *unbound* opens a pairing
window instead of switching, and runs no hooks either way.

There is no "deactivated" event; to undo something, hook the slot you switch back to. Nothing is
passed to the command: one hook belongs to one slot, so the hook itself is the identifier. Secrets
come from the environment km-hub runs in (`Environment=` / `EnvironmentFile=` in
`km-hub.service`), not from `config.toml`. Anything the command prints lands in the km-hub log.

Several hooks may share a slot and run concurrently. Like lighting, hooks can never delay or block a
switch: a missing, failing or slow command is logged and nothing more, and one that overruns its
timeout is killed. Config is read once at startup, so restart km-hub after editing hooks.

Note the Shift combo is swallowed whole, exactly like the plain one — no stray Shift reaches either
machine. Holding it for 3 s still means re-pair; that gesture is about the held F-key, not Shift.

## Run

```sh
make run                  # foreground, RUST_LOG=km_hub=debug, --transport le (Makefile default)
make run TRANSPORT=l2cap  # classic Bluetooth HID instead (needs a dual-mode controller: no LE-only step)
```

Then, on the Pi keyboard: `Ctrl+Alt+F2` → pairing window opens → **pair** from the client
(confirm the passkey) → `Ctrl+Alt+F2` again to switch to it. Repeat with `F3`, `F4`, … for
more clients. `Ctrl+Alt+F1` returns to the hub itself. Add `Shift` to any of these to run the
slot's hooks along with the switch.

### Transports

- `l2cap` — classic Bluetooth HID (the original transport). The hub is a *slave* the host polls;
  km-hub dials the active slot's host and answers HIDP control requests. Fine with BlueZ hosts.
- `le` — HID over GATT: the hub is a Bluetooth LE peripheral with HID, Battery and Device
  Information services and advertises permanently (visible to strangers only while a pairing window
  is open). Hosts connect to *us* whenever they see the advertisement — there is no dialing — and
  hold an 11.25–15 ms connection interval per link. This is what Apple's own keyboards and mice
  use, and what makes macOS/iOS pointers smooth; over classic HID those hosts poll an active-mode
  slave every 30–50 ms and the pointer lags. Switching transports means every host must **forget
  the hub and pair again**: the two bearers keep separate bonds. Run `make setup` first for the LE
  link tuning (and, recommended, the LE-only controller step).
- `log` — prints frames instead of sending; for wiring tests.

A systemd unit is provided (`km-hub.service`, `make install`) if you want it on boot.

## Troubleshooting

- *Client sees the device but gets no input* — it connected without bonding. Forget the device
  on the client, hold that slot's `Ctrl+Alt+F<n>` for 3 s to clear our side and reopen the window,
  then pair again. km-hub logs a warning and drops such links.
- *"cannot bind L2CAP PSM"* — bluetoothd's input plugin still owns the PSMs (`make setup`,
  step 3) or another km-hub instance is running.
- *LE: paired but no input* — the host has not enabled notifications yet. After pairing (and on
  every reconnect) the log should show `input report subscribed` three times (report IDs 1, 2, 4);
  without them frames are dropped as "host has not enabled this report". Forget and re-pair.
- *LE: the host paired the classic side* (shows a keyboard that never types) — the adapter is still
  dual-mode; run the `ControllerMode = le` step of `make setup`, forget the hub on the host, pair
  again.
- *LE: pointer still coarse* — check the negotiated interval: `sudo btmon` shows
  `LE Connection Update Complete` with the interval; it should be 11.25–15 ms. If it stays at the
  host's default (30 ms+), check `systemctl status km-hub-linktune` and the debugfs values it sets.
- *Colors don't change* — check the server is up (`systemctl status openrgb`, `ss -ltnp | grep
  6742`) and that km-hub logged `openrgb connected`. Run with `RUST_LOG=km_hub=debug` to see which
  devices and modes it picked up; a device whose modes take neither per-LED nor mode-specific
  colors is skipped.
- *A hook doesn't fire* — first, are you holding **Shift**? A plain `Ctrl+Alt+F<n>` switches without
  running anything. Then check km-hub logged `slot hooks armed` at startup (no `[[hook]]` entries
  means the task never starts) and that you restarted after editing the config. Run with
  `RUST_LOG=km_hub=debug` to see each hook spawn and exit. `$VARS` only expand in the string form;
  in the array form they are literal. A hook inherits km-hub's environment, so a token set only in
  your login shell won't be there — put it in `km-hub.service`.
- *Mouse feels sluggish on a client* — check the mouse's own report rate first
  (`sudo cat /dev/hidrawN | …`); a 62.5–125 Hz office mouse is the ceiling regardless of the link.

## Layout

```
src/main.rs        wiring: config, input readers, router, Bluetooth task
src/router.rs      hotkey FSM → local sink or HID translator; mouse pacing; slot switching
src/input/         evdev grab + uinput passthrough, hotkey detection
src/hid/           report descriptor, evdev → HID report translation
src/bt/            BlueZ session, pairing windows, slot bindings; L2CAP HID and LE/GATT transports
src/rgb/           slot lighting: OpenRGB SDK client + per-slot color state machine
src/hooks.rs       user commands run on Ctrl+Alt+Shift+F<n>
src/config.rs      config.toml         src/state.rs   state.toml (learned bindings)
setup.sh           system prerequisites (idempotent, --undo)
Makefile           cross-build + upload over ssh (parameters from .env, see .env.example)
.cargo/config.toml aarch64 linker for the cross-build
```
