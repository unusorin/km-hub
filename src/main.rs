mod bt;
mod config;
mod hid;
mod hooks;
mod input;
mod macro_cli;
mod rgb;
mod router;
mod state;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use bt::TransportKind;
use config::{LOCAL_SLOT, Settings};
use router::{Router, RouterDeps, RouterMsg};

struct Args {
    config: PathBuf,
    transport: TransportKind,
}

/// What the process was asked to do: run the hub, or one of the `macro` tools.
enum Invocation {
    Run(Args),
    Macro(Vec<String>),
}

fn parse_args() -> Result<Invocation> {
    let mut config = PathBuf::from("config.toml");
    let mut transport = TransportKind::Log;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                config = it.next().context("--config needs a path")?.into();
            }
            "--transport" => match it.next().as_deref() {
                Some("log") => transport = TransportKind::Log,
                Some("l2cap") => transport = TransportKind::L2cap,
                Some("le") => transport = TransportKind::Le,
                other => bail!("--transport must be 'log', 'l2cap' or 'le', got {other:?}"),
            },
            // `km-hub macro …` is a separate tool sharing the binary; it takes
            // its own flags, so hand it everything that follows (plus a
            // --config given before it, so both orders work).
            "macro" => {
                let mut rest = vec!["--config".to_string(), config.display().to_string()];
                rest.extend(it);
                return Ok(Invocation::Macro(rest));
            }
            "--help" | "-h" => {
                println!(
                    "usage: km-hub [--config <path>] [--transport log|l2cap|le]\n\
                     \x20      km-hub macro capture|add [--config <path>]\n\
                     \n\
                     --config     config file (default: ./config.toml, see config.example.toml)\n\
                     --transport  'log' prints HID frames instead of sending (default),\n\
                     \x20            'l2cap' streams over classic Bluetooth HID sockets,\n\
                     \x20            'le' serves HID over GATT as a Bluetooth LE peripheral\n\
                     \n\
                     macro capture  print the name of the next key combo you press\n\
                     macro add      capture a combo and write a [[macro]] entry into the config"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (see --help)"),
        }
    }
    Ok(Invocation::Run(Args { config, transport }))
}

/// Fail early with actionable messages instead of confusing errors later.
fn preflight() -> Result<()> {
    let uinput = std::path::Path::new("/dev/uinput");
    if !uinput.exists() {
        bail!("/dev/uinput missing — load the uinput kernel module (modprobe uinput)");
    }
    if std::fs::OpenOptions::new().write(true).open(uinput).is_err() {
        bail!(
            "/dev/uinput not writable — run ./setup.sh (udev rule + input group), \
             then log out and back in"
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = match parse_args()? {
        Invocation::Run(args) => args,
        Invocation::Macro(rest) => return macro_cli::run(rest).await,
    };
    let settings = Settings::load(&args.config)?;
    let state_path = args
        .config
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("state.toml");
    let bindings = state::load(&state_path, &settings.targets)?;
    info!(
        config = %args.config.display(),
        state = %state_path.display(),
        bound_slots = bindings.len(),
        "km-hub starting"
    );
    preflight()?;

    let cancel = CancellationToken::new();
    let (router_tx, router_rx) = mpsc::channel::<RouterMsg>(256);
    // Bounded + try_send on the sender side: a stalled BT link drops frames
    // instead of back-pressuring input handling.
    let (frame_tx, frame_rx) = mpsc::channel::<(u8, hid::HidFrame)>(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<bt::BtCmd>(8);
    let (slot_tx, slot_rx) = watch::channel(LOCAL_SLOT);
    let (bindings_tx, bindings_rx) = watch::channel(bindings.clone());

    // Sink first: discovery must see the virtual devices to exclude them.
    let sink = input::LocalSink::new()?;
    let devices = input::discover(&settings.devices)?;
    let reader_handles = input::spawn_readers(devices, router_tx, cancel.clone())?;
    info!(readers = reader_handles.len(), "input devices grabbed");

    // Lighting is opt-in and strictly cosmetic; without [rgb] enabled the task
    // never starts and the Bluetooth side has nobody to notify.
    let (rgb_tx, activity, rgb_handle) = if settings.rgb.enabled {
        let (tx, event_rx) = mpsc::channel::<rgb::RgbEvent>(8);
        // Seeded with the launch instant, so the LEDs are lit at startup and
        // idle out from here if nobody is at the desk.
        let (activity_tx, activity_rx) = watch::channel(Instant::now());
        let handle = tokio::spawn(rgb::run(rgb::RgbDeps {
            settings: settings.rgb.clone(),
            slot_rx: slot_rx.clone(),
            activity_rx,
            event_rx,
            cancel: cancel.clone(),
        }));
        (Some(tx), Some(rgb::Activity::new(activity_tx)), Some(handle))
    } else {
        (None, None, None)
    };

    // Hooks and macros are opt-in the same way: with no [[hook]] or [[macro]]
    // entries the task never starts and the router has nobody to notify.
    let (hook_tx, hook_handle) = if settings.hooks.is_empty() && settings.macros.is_empty() {
        (None, None)
    } else {
        let (tx, event_rx) = mpsc::channel::<hooks::HookEvent>(8);
        let handle = tokio::spawn(hooks::run(hooks::HookDeps {
            hooks: settings.hooks.clone(),
            macros: settings.macros.clone(),
            event_rx,
            cancel: cancel.clone(),
        }));
        (Some(tx), Some(handle))
    };

    let router = Router::new(RouterDeps {
        sink,
        bt_tx: frame_tx,
        bt_cmd_tx: cmd_tx,
        slot_tx,
        hook_tx,
        macros: settings.macros.iter().map(|m| m.combo).collect(),
        activity,
        // Cloned: the Bluetooth task keeps the original for pairing and bind
        // events, the router uses its copy to ask for a repaint.
        rgb_tx: rgb_tx.clone(),
        bindings_rx,
        mouse_rate_hz: settings.mouse_rate_hz,
    });
    let router_handle = tokio::spawn(router.run(router_rx, cancel.clone()));
    let macro_count = settings.macros.len();
    let deps = bt::BtDeps {
        settings,
        kind: args.transport,
        frame_rx,
        cmd_rx,
        slot_rx,
        bindings_tx,
        bindings,
        state_path,
        rgb_tx,
        cancel: cancel.clone(),
    };
    // Report a Bluetooth failure the moment it happens, not at shutdown: at
    // boot this task can lose the race against bluetoothd, and a hub with no
    // Bluetooth is not worth keeping alive silently.
    let (bt_done_tx, bt_done_rx) = tokio::sync::oneshot::channel();
    let bt_handle = tokio::spawn(async move {
        let result = bt::run(deps).await;
        match &result {
            Err(err) => error!(err = format!("{err:#}"), "bluetooth manager failed"),
            Ok(()) => info!("bluetooth manager stopped"),
        }
        let _ = bt_done_tx.send(());
        result
    });

    info!(
        "running — Ctrl+Alt+F1 = local, Ctrl+Alt+F<n> = bound slot n \
         (unbound: opens a pairing window), add Shift to also run the slot's \
         hooks, Ctrl-C to quit"
    );
    if macro_count > 0 {
        info!(macros = macro_count, "macro combos armed on every slot");
    }
    // Exiting non-zero lets the supervisor restart us once BlueZ is ready.
    let bt_died = tokio::select! {
        res = tokio::signal::ctrl_c() => {
            res?;
            info!("shutting down");
            false
        }
        _ = bt_done_rx => {
            error!("bluetooth task ended — exiting so the service manager can restart us");
            true
        }
    };
    cancel.cancel();

    let shutdown = async {
        for h in reader_handles {
            let _ = h.await;
        }
        match router_handle.await {
            Ok(Err(err)) => error!(%err, "router failed"),
            Err(err) => error!(%err, "router task panicked"),
            _ => {}
        }
        // Failures were already logged where they happened.
        if let Err(err) = bt_handle.await {
            error!(%err, "bluetooth task panicked");
        }
        if let Some(handle) = rgb_handle {
            match handle.await {
                Ok(Err(err)) => error!(%err, "lighting failed"),
                Err(err) => error!(%err, "lighting task panicked"),
                _ => {}
            }
        }
        if let Some(handle) = hook_handle {
            match handle.await {
                Ok(Err(err)) => error!(%err, "hooks failed"),
                Err(err) => error!(%err, "hook task panicked"),
                _ => {}
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(3), shutdown).await.is_err() {
        warn!("shutdown timed out, exiting anyway");
    }
    if bt_died {
        bail!("bluetooth manager is gone");
    }
    Ok(())
}
