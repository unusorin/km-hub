# km-hub — cross-build locally, upload to the Raspberry Pi hub over ssh.
#
#   make build     cargo build --release for $(TARGET) (aarch64) on this machine
#   make sync      rsync setup.sh / km-hub.service / config*.toml / README.md to the Pi
#   make deploy    build + sync + upload the binary + setcap (no restart)
#   make setup     run setup.sh on the Pi (udev, bluetoothd -P input, class)
#   make run       run the daemon in the foreground on the Pi (Ctrl-C to stop)
#   make install   install + enable a systemd unit on the Pi
#   make restart / stop / logs / status
#   make ssh       open a shell on the Pi
#
# Per-machine parameters (PI, REMOTE_DIR, TARGET, TRANSPORT, RUST_LOG) come from
# .env — `cp .env.example .env` and edit. Command-line overrides still win:
#   make deploy PI=admin@other-host
#
# Host prerequisites for the cross-build:
#   rustup target add aarch64-unknown-linux-gnu
#   sudo apt install gcc-aarch64-linux-gnu        (linker; see .cargo/config.toml)

-include .env

REMOTE_DIR ?= km-hub
TARGET     ?= aarch64-unknown-linux-gnu
TRANSPORT  ?= le
RUST_LOG   ?= km_hub=debug

BIN        = target/$(TARGET)/release/km-hub
REMOTE_BIN = $(REMOTE_DIR)/km-hub
SSH        = ssh -o BatchMode=yes $(PI)
# Files the Pi needs besides the binary. state.toml is written by the daemon
# on the Pi and never uploaded.
SUPPORT    = setup.sh km-hub.service config.toml config.example.toml README.md

.PHONY: check build sync deploy setup run install restart stop status logs ssh clean-remote pi-required

check:
	cargo check

build:
	cargo build --release --target $(TARGET)

# Every target that talks to the Pi depends on this so a missing .env fails
# with a hint instead of ssh'ing to nowhere.
pi-required:
	$(if $(PI),,$(error PI is not set — cp .env.example .env and edit it, or pass PI=user@host))

sync: pi-required
	rsync -az --info=stats1 $(SUPPORT) $(PI):$(REMOTE_DIR)/

deploy: build sync
	rsync -az --info=stats1 $(BIN) $(PI):$(REMOTE_BIN)
	$(SSH) 'sudo setcap cap_net_bind_service+ep $(REMOTE_BIN) && ls -l $(REMOTE_BIN)'

setup: sync
	ssh -t $(PI) 'cd $(REMOTE_DIR) && ./setup.sh'

run: deploy
	ssh -t $(PI) 'cd $(REMOTE_DIR) && sudo systemctl stop km-hub 2>/dev/null; RUST_LOG=$(RUST_LOG) ./km-hub --transport $(TRANSPORT)'

# 'enable --now' is a no-op on an already-running service, so a changed unit or
# binary would silently keep running the old one: restart explicitly.
install: deploy
	$(SSH) 'cd $(REMOTE_DIR) && sudo install -m644 km-hub.service /etc/systemd/system/km-hub.service \
		&& sudo systemctl daemon-reload && sudo systemctl enable km-hub && sudo systemctl restart km-hub \
		&& systemctl status --no-pager km-hub | head -5'

restart: pi-required
	$(SSH) 'sudo systemctl restart km-hub && systemctl status --no-pager km-hub | head -5'

stop: pi-required
	$(SSH) 'sudo systemctl stop km-hub'

status: pi-required
	$(SSH) 'systemctl status --no-pager km-hub; hciconfig -a | head -8'

logs: pi-required
	$(SSH) 'journalctl -u km-hub -f -n 100'

ssh: pi-required
	ssh -t $(PI) 'cd $(REMOTE_DIR); exec $$SHELL -l'

# Leftover from when the build ran on the Pi.
clean-remote: pi-required
	$(SSH) 'rm -rf $(REMOTE_DIR)/target'
