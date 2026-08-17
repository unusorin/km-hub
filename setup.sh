#!/usr/bin/env bash
# One-time system setup for km-hub. Idempotent; prompts before each step.
# Run as your normal user: it invokes sudo where needed.
set -euo pipefail

confirm() {
    read -r -p "$1 [y/N] " reply
    [[ "$reply" =~ ^[Yy]$ ]]
}

# OpenRGB is not packaged by Debian; upstream ships per-arch/-release .debs.
OPENRGB_RELEASE=1.0rc3
OPENRGB_BUILD=6fbcf62
OPENRGB_DOWNLOADS="https://codeberg.org/OpenRGB/OpenRGB/releases/download/release_candidate_${OPENRGB_RELEASE}"
OPENRGB_UNIT=/etc/systemd/system/openrgb.service
OPENRGB_RULES=/etc/udev/rules.d/60-openrgb.rules

# --undo: revert the bluetoothd changes (drop-in + adapter class). Use this on
# a machine that ran setup.sh but is now a *client* of the hub — with '-P input'
# bluetoothd cannot act as an HID host, so it will never accept our keyboard.
if [[ "${1:-}" == "--undo" ]]; then
    echo "== km-hub undo (bluetoothd drop-in/patch + adapter class + LE-only mode + link tuning + OpenRGB service) =="
    if [[ -f "$OPENRGB_UNIT" ]]; then
        sudo systemctl disable --now openrgb
        sudo rm -f "$OPENRGB_UNIT"
        sudo systemctl daemon-reload
        echo "   -> openrgb service removed (the package and udev rules are left in place;"
        echo "      'sudo apt remove openrgb' and 'sudo rm $OPENRGB_RULES' to go further)."
    fi
    if [[ -f /etc/systemd/system/km-hub-linktune.service ]]; then
        sudo systemctl disable --now km-hub-linktune
        sudo rm -f /etc/systemd/system/km-hub-linktune.service
        echo "   -> link tuning removed (adapters revert to their defaults on next boot)."
    fi
    if dpkg-divert --list /usr/libexec/bluetooth/bluetoothd | grep -q .; then
        sudo rm -f /usr/libexec/bluetooth/bluetoothd
        sudo dpkg-divert --remove --rename /usr/libexec/bluetooth/bluetoothd
        echo "   -> stock bluetoothd restored."
    fi
    sudo rm -f /etc/systemd/system/bluetooth.service.d/km-hub.conf
    sudo rmdir /etc/systemd/system/bluetooth.service.d 2>/dev/null || true
    sudo sed -i '/^Class *= *0x0005[Cc]0/d' /etc/bluetooth/main.conf
    sudo sed -i '/^ControllerMode *= *le/d' /etc/bluetooth/main.conf
    sudo systemctl daemon-reload
    sudo systemctl restart bluetooth
    sleep 2
    read -r -p "Adapter alias to restore (empty = keep '$(bluetoothctl show | sed -n 's/^\tAlias: //p' | head -1)'): " alias
    [[ -n "$alias" ]] && bluetoothctl system-alias "$alias" >/dev/null
    echo "== undo complete (udev rules / input group left in place) =="
    exit 0
fi

echo "== km-hub system setup =="

# 1. udev rules + input group for /dev/input/event* and /dev/uinput access
RULES=/etc/udev/rules.d/99-km-hub.rules
if confirm "Install udev rules ($RULES) and add $USER to the 'input' group?"; then
    sudo tee "$RULES" >/dev/null <<'EOF'
KERNEL=="event*", GROUP="input", MODE="0660"
KERNEL=="uinput", GROUP="input", MODE="0660"
EOF
    sudo usermod -aG input "$USER"
    # uinput is a module on some images (Raspberry Pi OS); the static devnode
    # keeps root-only perms until the module is loaded and udev sees it.
    sudo modprobe uinput
    echo uinput | sudo tee /etc/modules-load.d/km-hub.conf >/dev/null
    sudo udevadm control --reload-rules
    sudo udevadm trigger
    echo "   -> done. Log out and back in for the group change to take effect."
fi

# 1b. Some images ship Bluetooth rfkill soft-blocked (adapter shows
#     PowerState: off-blocked). Unblock; systemd-rfkill persists the state.
for rf in /sys/class/rfkill/rfkill*; do
    if [[ -e "$rf/type" && "$(cat "$rf/type")" == bluetooth && "$(cat "$rf/soft")" == 1 ]]; then
        if confirm "Bluetooth is rfkill soft-blocked; unblock it?"; then
            echo 0 | sudo tee "$rf/soft" >/dev/null
            echo "   -> done."
        fi
    fi
done

# 2. Disable bluetoothd plugins we must not (or should not) run:
#      input  - binds the HID PSMs 0x11/0x13 in *host* role, which blocks a
#               userspace HID device implementation like ours
#      sap    - SIM Access; advertises a phone profile and sets the Telephony
#               bit in our class of device
#      avrcp  - A/V Remote Control (target + controller)
#      a2dp   - audio streaming
#    A keyboard/mouse has no business offering the last three, and a host that
#    sees them classifies the hub as something other than a plain HID peripheral.
DROPIN_DIR=/etc/systemd/system/bluetooth.service.d
BLUEZ_DISABLED_PLUGINS="input,sap,avrcp,a2dp"
if confirm "Disable bluetoothd plugins ($BLUEZ_DISABLED_PLUGINS) via systemd drop-in and restart bluetooth?"; then
    EXEC=$(systemctl cat bluetooth | grep -m1 '^ExecStart=' | sed 's/^ExecStart=//')
    CURRENT=$(systemctl show bluetooth -p ExecStart --value 2>/dev/null | grep -o -- '-P [a-z0-9,]*' | head -1)
    if [[ "$CURRENT" == "-P $BLUEZ_DISABLED_PLUGINS" ]]; then
        echo "   -> already configured."
    else
        sudo mkdir -p "$DROPIN_DIR"
        sudo tee "$DROPIN_DIR/km-hub.conf" >/dev/null <<EOF
[Service]
ExecStart=
ExecStart=$EXEC -P $BLUEZ_DISABLED_PLUGINS
EOF
        sudo systemctl daemon-reload
        sudo systemctl restart bluetooth
        echo "   -> done."
    fi
fi

# 3. Device class: keyboard/mouse combo peripheral so macOS classifies us as
#    an input device instead of a PC. Persisted via main.conf (hciconfig
#    changes are lost on every bluetoothd restart).
MAINCONF=/etc/bluetooth/main.conf
if confirm "Set adapter class to 0x0005C0 (keyboard/mouse combo) in $MAINCONF?"; then
    if grep -qE '^Class *= *0x0005[Cc]0' "$MAINCONF"; then
        echo "   -> already configured."
    else
        sudo sed -i '/^Class *=/d' "$MAINCONF"
        sudo sed -i '0,/^\[General\]/s//[General]\nClass = 0x0005C0/' "$MAINCONF"
        sudo systemctl restart bluetooth
        echo "   -> done (persistent)."
    fi
fi

# 3b. Link power management. A HID device wants its ACL link in *sniff* mode:
#     sniff reserves anchor points in the host's baseband schedule, whereas an
#     active link is best-effort and gets starved whenever the host's radio is
#     busy (opening its Bluetooth settings pane, for one). Real mice and
#     keyboards all do this; km-hub used to opt out of it and stuttered.
#
#     Three kernel-side gates, none of which BlueZ sets for us:
#       link policy    - must include sniff, or the controller refuses the
#                        host's sniff request outright
#       idle_timeout   - milliseconds of inactivity before entering sniff; 0
#                        (the default) means the transition is never scheduled
#       sniff interval - the anchor point spacing, in units of 0.625 ms and
#                        always even. The kernel defaults (80/800 = 50-500 ms)
#                        are sized for file transfer and are catastrophic for a
#                        pointer: they put a floor under input latency equal to
#                        the interval. 12/18 = 7.5-11.25 ms matches what
#                        commercial mice negotiate. Sniff attempt and timeout
#                        are hardcoded in the kernel and cannot be tuned here.
#     All three are per-boot, hence the unit. The socket side is in transport.rs.
#
#     The same unit carries the LE side (--transport le). As an LE peripheral
#     the kernel itself asks the host for a connection interval inside
#     [conn_min_interval, conn_max_interval] (units of 1.25 ms) right after
#     the link comes up; the defaults (24/40 = 30-50 ms) are a file-transfer
#     choice again. 9/12 = 11.25-15 ms is Apple's HID exception to its own
#     "max >= min + 15 ms" rule and what its keyboards and mice run at.
#     supervision_timeout is in 10 ms units; Apple rejects anything under 2 s
#     (the kernel default is 420 ms). adv_*_interval (0.625 ms units) default
#     to 1.28 s, which makes discovery and reconnects sluggish; 100-150 ms is
#     the usual peripheral choice. These could also live in main.conf [LE];
#     keep them here so one unit owns every knob — do not set both.
LINKTUNE_UNIT=/etc/systemd/system/km-hub-linktune.service
LINKTUNE_IDLE_MS=${LINKTUNE_IDLE_MS:-2000}
LINKTUNE_SNIFF_MIN=${LINKTUNE_SNIFF_MIN:-12}
LINKTUNE_SNIFF_MAX=${LINKTUNE_SNIFF_MAX:-18}
LINKTUNE_LE_MIN=${LINKTUNE_LE_MIN:-9}
LINKTUNE_LE_MAX=${LINKTUNE_LE_MAX:-12}
LINKTUNE_LE_LATENCY=${LINKTUNE_LE_LATENCY:-0}
LINKTUNE_LE_SUPV=${LINKTUNE_LE_SUPV:-200}
LINKTUNE_ADV_MIN=${LINKTUNE_ADV_MIN:-160}
LINKTUNE_ADV_MAX=${LINKTUNE_ADV_MAX:-240}
if confirm "Tune link parameters on every adapter (sniff idle=${LINKTUNE_IDLE_MS}ms ${LINKTUNE_SNIFF_MIN}/${LINKTUNE_SNIFF_MAX} slots; LE interval ${LINKTUNE_LE_MIN}/${LINKTUNE_LE_MAX} x1.25ms, timeout ${LINKTUNE_LE_SUPV}0ms, adv ${LINKTUNE_ADV_MIN}/${LINKTUNE_ADV_MAX} slots) for HID latency?"; then
    sudo tee "$LINKTUNE_UNIT" >/dev/null <<EOF
[Unit]
Description=km-hub Bluetooth link tuning (sniff mode + LE connection interval for HID latency)
After=bluetooth.service
BindsTo=bluetooth.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'for d in /sys/class/bluetooth/hci*; do \
  h=\$(basename "\$d"); \
  hciconfig "\$h" lp rswitch,sniff || true; \
  b=/sys/kernel/debug/bluetooth/\$h; \
  [ -w "\$b/idle_timeout" ] && echo $LINKTUNE_IDLE_MS > "\$b/idle_timeout"; \
  [ -w "\$b/sniff_min_interval" ] && echo $LINKTUNE_SNIFF_MIN > "\$b/sniff_min_interval"; \
  [ -w "\$b/sniff_max_interval" ] && echo $LINKTUNE_SNIFF_MAX > "\$b/sniff_max_interval"; \
  [ -w "\$b/conn_min_interval" ] && echo $LINKTUNE_LE_MIN > "\$b/conn_min_interval"; \
  [ -w "\$b/conn_max_interval" ] && echo $LINKTUNE_LE_MAX > "\$b/conn_max_interval"; \
  [ -w "\$b/conn_latency" ] && echo $LINKTUNE_LE_LATENCY > "\$b/conn_latency"; \
  [ -w "\$b/supervision_timeout" ] && echo $LINKTUNE_LE_SUPV > "\$b/supervision_timeout"; \
  [ -w "\$b/adv_min_interval" ] && echo $LINKTUNE_ADV_MIN > "\$b/adv_min_interval"; \
  [ -w "\$b/adv_max_interval" ] && echo $LINKTUNE_ADV_MAX > "\$b/adv_max_interval"; \
  true; \
done'

[Install]
WantedBy=bluetooth.service
EOF
    sudo systemctl daemon-reload
    sudo systemctl enable --now km-hub-linktune
    echo "   -> done. Existing links keep their old policy; reconnect a client to apply."
    for d in /sys/class/bluetooth/hci*; do
        h=$(basename "$d")
        b=/sys/kernel/debug/bluetooth/$h
        echo "      $h: $(hciconfig "$h" | sed -n 's/.*Link policy: /policy=/p')" \
            "idle=$(sudo cat "$b/idle_timeout" 2>/dev/null || echo n/a)ms" \
            "sniff=$(sudo cat "$b/sniff_min_interval" 2>/dev/null || echo n/a)/$(sudo cat "$b/sniff_max_interval" 2>/dev/null || echo n/a)" \
            "le=$(sudo cat "$b/conn_min_interval" 2>/dev/null || echo n/a)/$(sudo cat "$b/conn_max_interval" 2>/dev/null || echo n/a)" \
            "supv=$(sudo cat "$b/supervision_timeout" 2>/dev/null || echo n/a)" \
            "adv=$(sudo cat "$b/adv_min_interval" 2>/dev/null || echo n/a)/$(sudo cat "$b/adv_max_interval" 2>/dev/null || echo n/a)"
    done
fi

# 3c. LE-only controller, for --transport le. With ControllerMode = le the
#     adapter stops page-scanning and its advertisements carry "BR/EDR Not
#     Supported", so a dual-mode host (macOS, iOS) cannot pair or reconnect the
#     classic side it has no HID service on, and cannot keep a stale classic
#     bond around. Mutually exclusive with --transport l2cap: undo this (or
#     ./setup.sh --undo) before going back to classic HID.
if confirm "LE-only controller (ControllerMode = le in $MAINCONF) — required for --transport le, breaks --transport l2cap?"; then
    if grep -qE '^ControllerMode *= *le' "$MAINCONF"; then
        echo "   -> already configured."
    else
        sudo sed -i '/^ControllerMode *=/d' "$MAINCONF"
        sudo sed -i '0,/^\[General\]/s//[General]\nControllerMode = le/' "$MAINCONF"
        sudo systemctl restart bluetooth
        echo "   -> done (persistent). Hosts paired over classic HID must forget the hub and pair again over LE."
    fi
fi

# 3d. Patched bluetoothd, for --transport le. Stock bluetoothd remembers a
#     bonded host's "notifications on" (CCC) across disconnects and treats the
#     host's re-enable on reconnect as a no-op, so it never hands km-hub a new
#     notify session: the host (rebooted PC, Mac after sleep, phone back in
#     range) shows connected but hears nothing. Working around it from the
#     application (re-registering the GATT services) trades that for another
#     BlueZ bug on Linux hosts (the on-disk record loses HID and auto-connect
#     dies). patches/ makes bluetoothd forget those values when the link goes
#     down; the reconnecting host re-enables them and everything follows.
#     Built from the upstream tarball matching the installed version, only the
#     daemon binary is replaced, via dpkg-divert so package upgrades keep the
#     diversion (an upgrade to a new version does need this step again).
BLUEZ_VER=$(/usr/libexec/bluetooth/bluetoothd -v 2>/dev/null || true)
BLUEZ_PATCH="$(realpath "$(dirname "$0")")/patches/bluez-${BLUEZ_VER}-forget-ccc-on-disconnect.patch"
BLUEZ_BIN=/usr/libexec/bluetooth/bluetoothd
if [[ -f "$BLUEZ_PATCH" ]] &&
    confirm "Build and install bluetoothd $BLUEZ_VER with the km-hub CCC patch (dpkg-divert, restarts bluetooth)?"; then
    if grep -q "km-hub CCC patch" "$BLUEZ_BIN" 2>/dev/null; then
        echo "   -> already installed."
    else
        sudo apt-get install -y -q build-essential pkg-config libglib2.0-dev libdbus-1-dev libudev-dev
        BUILD=$HOME/bluez-build
        mkdir -p "$BUILD" && cd "$BUILD"
        [[ -f "bluez-${BLUEZ_VER}.tar.xz" ]] || curl -sSLO "https://www.kernel.org/pub/linux/bluetooth/bluez-${BLUEZ_VER}.tar.xz"
        rm -rf "bluez-${BLUEZ_VER}"
        tar xf "bluez-${BLUEZ_VER}.tar.xz"
        cd "bluez-${BLUEZ_VER}"
        patch -p1 < "$BLUEZ_PATCH"
        # Marker so a rerun (and a human) can tell the binary apart.
        echo 'const char km_hub_marker[] = "km-hub CCC patch";' >> src/main.c
        ./configure --prefix=/usr --libexecdir=/usr/libexec --sysconfdir=/etc --localstatedir=/var \
            --disable-client --disable-obex --disable-cups --disable-tools --disable-monitor \
            --disable-hid2hci --disable-udev --disable-systemd --disable-manpages --disable-datafiles >/dev/null
        make -j"$(nproc)" >/dev/null
        cd - >/dev/null
        sudo dpkg-divert --add --rename --divert "${BLUEZ_BIN}.distrib" "$BLUEZ_BIN"
        sudo install -m755 "$BUILD/bluez-${BLUEZ_VER}/src/bluetoothd" "$BLUEZ_BIN"
        sudo systemctl restart bluetooth
        echo "   -> done: $BLUEZ_BIN is the patched build (stock binary kept as ${BLUEZ_BIN}.distrib)."
    fi
fi

# 4. Optional: allow binding privileged L2CAP PSMs without running as root.
BIN=${BIN:-km-hub}
if [[ -f "$BIN" ]] && confirm "Grant cap_net_bind_service to $BIN?"; then
    sudo setcap cap_net_bind_service+ep "$BIN"
    echo "   -> done (re-run after each rebuild)."
fi

# 5. Optional: OpenRGB, for the slot lighting configured under [rgb] in
#    config.toml. Debian does not package it, so this pulls the upstream .deb
#    matching this machine's architecture and release.
if command -v openrgb >/dev/null; then
    echo "-- OpenRGB already installed ($(openrgb --version 2>/dev/null | head -1))."
elif confirm "Install OpenRGB $OPENRGB_RELEASE (slot lighting for the hub's keyboard/mouse)?"; then
    ARCH=$(dpkg --print-architecture)
    CODENAME=$(. /etc/os-release && echo "${VERSION_CODENAME:-}")
    DEB="openrgb_${OPENRGB_RELEASE}_${ARCH}_${CODENAME}_${OPENRGB_BUILD}.deb"
    TMP=$(mktemp -d)
    if curl -fsSL -o "$TMP/$DEB" "$OPENRGB_DOWNLOADS/$DEB" &&
        curl -fsSL -o "$TMP/60-openrgb.rules" "$OPENRGB_DOWNLOADS/60-openrgb.rules"; then
        sudo apt install -y "$TMP/$DEB"
        # Device access rules (they cover the hub's keyboard and mouse).
        sudo install -m644 "$TMP/60-openrgb.rules" "$OPENRGB_RULES"
        sudo udevadm control --reload-rules
        sudo udevadm trigger
        echo "   -> done."
    else
        echo "   -> no upstream build for ${ARCH}/${CODENAME}; see" \
            "https://codeberg.org/OpenRGB/OpenRGB/releases (skipping)."
    fi
    rm -rf "$TMP"
fi

# 5b. Run the OpenRGB SDK server km-hub talks to. Root on purpose: OpenRGB's
#     udev rules grant access through 'uaccess', which only applies to a
#     seat-local login session — a hub reached over ssh has none.
if command -v openrgb >/dev/null && [[ ! -f "$OPENRGB_UNIT" ]] &&
    confirm "Run the OpenRGB SDK server on 127.0.0.1:6742 as a service?"; then
    sudo tee "$OPENRGB_UNIT" >/dev/null <<EOF
[Unit]
Description=OpenRGB SDK server (km-hub slot lighting)
After=network.target

[Service]
Type=simple
ExecStart=$(command -v openrgb) --server --server-host 127.0.0.1 --server-port 6742 --noautoconnect
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
    sudo systemctl daemon-reload
    sudo systemctl enable --now openrgb
    sleep 1
    systemctl status --no-pager openrgb | head -5
fi

echo "== setup complete =="
