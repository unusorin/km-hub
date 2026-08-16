//! Slot hooks and macros: user commands run when a slot is activated with
//! Ctrl+Alt+Shift+F<n> (`[[hook]]`) or when a configured key combo is pressed
//! (`[[macro]]`).
//!
//! km-hub knows nothing about what a hook does — it spawns the command and
//! forgets about it. That keeps the hub generic: pointing a hook at `curl`,
//! `hass-cli` or a shell script is the user's business, not the tool's.
//!
//! Same contract as the lighting task: a hook can neither delay nor block a
//! switch (or a keystroke). The router hands events over with `try_send` and
//! never waits, every command runs in its own task under a timeout, and every
//! failure is a log line and nothing more.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{Hook, HookCommand, Macro};

/// Ceiling on concurrently running hooks and macros together. Reached only if
/// commands hang while combos are pressed repeatedly; skipping beats forking
/// without bound on a Pi.
const MAX_IN_FLIGHT: usize = 16;

/// What the router asks the task to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// The slot the user asked to fire. One message per hook-combo press,
    /// whether or not it also changed the active target.
    Slot(u8),
    /// A macro combo was pressed; index into `HookDeps::macros`.
    Macro(usize),
}

pub struct HookDeps {
    pub hooks: Vec<Hook>,
    pub macros: Vec<Macro>,
    pub event_rx: mpsc::Receiver<HookEvent>,
    pub cancel: CancellationToken,
}

pub async fn run(deps: HookDeps) -> Result<()> {
    let HookDeps {
        hooks,
        macros,
        mut event_rx,
        cancel,
    } = deps;
    info!(hooks = hooks.len(), macros = macros.len(), "slot hooks and macros armed");

    let mut running: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            // Reap finished commands so `running.len()` is a live count.
            Some(_) = running.join_next() => {}
            event = event_rx.recv() => match event {
                Some(HookEvent::Slot(slot)) => spawn_matching(&hooks, slot, &mut running),
                Some(HookEvent::Macro(index)) => match macros.get(index) {
                    Some(m) => spawn_one(&m.label, &m.command, m.timeout, &mut running),
                    None => warn!(index, "macro index out of range — ignoring"),
                },
                None => break,
            },
        }
    }

    // Dropping the JoinSet kills what is still running (kill_on_drop): main
    // gives the whole shutdown 3 seconds, which a hung command must not eat.
    if !running.is_empty() {
        warn!(in_flight = running.len(), "shutting down with hooks still running — killing them");
    }
    info!("slot hooks and macros stopped");
    Ok(())
}

fn spawn_matching(hooks: &[Hook], slot: u8, running: &mut JoinSet<()>) {
    for hook in hooks.iter().filter(|h| h.slot == slot) {
        spawn_one(&hook.label, &hook.command, hook.timeout, running);
    }
}

fn spawn_one(label: &str, command: &HookCommand, timeout: Duration, running: &mut JoinSet<()>) {
    if running.len() >= MAX_IN_FLIGHT {
        warn!(
            hook = label,
            in_flight = running.len(),
            "too many hooks still running — skipping this one"
        );
        return;
    }
    let (label, command) = (label.to_string(), command.clone());
    running.spawn(async move { exec(&label, &command, timeout).await });
}

/// Run one command to completion (or to its timeout). Never returns an error:
/// a broken hook is the user's problem to see in the log, not the hub's to
/// propagate.
async fn exec(label: &str, command: &HookCommand, timeout: Duration) {
    let mut cmd = match command {
        // `sh -c` so the string may use $VARS, pipes and redirection.
        HookCommand::Shell(line) => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(line);
            cmd
        }
        HookCommand::Argv(argv) => {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            cmd
        }
    };
    // stdout/stderr are inherited on purpose: a hook's own output lands in the
    // km-hub log (journald under systemd), which is what makes it debuggable.
    cmd.stdin(Stdio::null()).kill_on_drop(true);

    debug!(hook = label, ?command, "running hook");
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!(hook = label, %err, "cannot run hook");
            return;
        }
    };

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => debug!(hook = label, "hook finished"),
        Ok(Ok(status)) => warn!(hook = label, %status, "hook exited with a failure"),
        Ok(Err(err)) => warn!(hook = label, %err, "cannot wait for hook"),
        Err(_) => {
            warn!(hook = label, timeout_secs = timeout.as_secs(), "hook timed out — killing it");
            // Best effort: the child is killed on drop anyway.
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No `tempfile` dependency, and tests share a process: key marker paths by
    /// pid *and* test name so parallel runs cannot collide.
    fn marker(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("km-hub-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn hook(slot: u8, command: HookCommand) -> Hook {
        Hook {
            slot,
            label: format!("test slot {slot}"),
            command,
            timeout: Duration::from_secs(5),
        }
    }

    fn argv(parts: &[&str]) -> HookCommand {
        HookCommand::Argv(parts.iter().map(|p| (*p).to_string()).collect())
    }

    /// Drives the task until `path` shows up, so tests never race the spawn.
    async fn wait_for(path: &std::path::Path) -> bool {
        for _ in 0..100 {
            if path.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    async fn start(hooks: Vec<Hook>) -> (mpsc::Sender<HookEvent>, CancellationToken) {
        start_with(hooks, Vec::new()).await
    }

    async fn start_with(
        hooks: Vec<Hook>,
        macros: Vec<Macro>,
    ) -> (mpsc::Sender<HookEvent>, CancellationToken) {
        let (tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        tokio::spawn(run(HookDeps {
            hooks,
            macros,
            event_rx,
            cancel: cancel.clone(),
        }));
        (tx, cancel)
    }

    fn a_macro(keys: &str, command: HookCommand) -> Macro {
        Macro {
            combo: keys.parse().unwrap(),
            label: format!("test macro {keys}"),
            command,
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn a_hook_runs_when_its_slot_activates() {
        let path = marker("runs");
        let (tx, cancel) = start(vec![hook(2, argv(&["touch", &path.to_string_lossy()]))]).await;
        tx.send(HookEvent::Slot(2)).await.unwrap();
        assert!(wait_for(&path).await, "hook did not run");
        cancel.cancel();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn only_hooks_for_the_activated_slot_run() {
        let (two, three) = (marker("only-2"), marker("only-3"));
        let (tx, cancel) = start(vec![
            hook(2, argv(&["touch", &two.to_string_lossy()])),
            hook(3, argv(&["touch", &three.to_string_lossy()])),
        ])
        .await;
        tx.send(HookEvent::Slot(3)).await.unwrap();
        assert!(wait_for(&three).await, "slot 3 hook did not run");
        // Slot 3's hook is done, so slot 2's would have run by now if it were
        // going to.
        assert!(!two.exists(), "slot 2 hook ran for a slot 3 switch");
        cancel.cancel();
        let _ = std::fs::remove_file(&three);
    }

    /// The whole point of the two forms: a string gets a shell, an array does
    /// not. Demonstrated without touching the process environment, which is not
    /// safe to mutate from a test that shares a process with the others.
    #[tokio::test]
    async fn only_the_shell_form_is_interpreted() {
        // Shell: a variable and && only mean anything to `sh`.
        let expanded = marker("interp-shell");
        // Argv: one argument containing a space stays one filename; a shell
        // would have split it into two.
        let one_arg = marker("interp argv");
        let split = marker("interp");

        let (tx, cancel) = start(vec![
            hook(
                2,
                HookCommand::Shell(format!("p='{}' && touch \"$p\"", expanded.display())),
            ),
            hook(2, argv(&["touch", &one_arg.to_string_lossy()])),
        ])
        .await;
        tx.send(HookEvent::Slot(2)).await.unwrap();

        assert!(wait_for(&expanded).await, "shell form was not interpreted by sh");
        assert!(wait_for(&one_arg).await, "argv form did not create the spaced filename");
        assert!(!split.exists(), "argv form was word-split as if by a shell");

        cancel.cancel();
        let _ = std::fs::remove_file(&expanded);
        let _ = std::fs::remove_file(&one_arg);
    }

    #[tokio::test]
    async fn a_broken_hook_does_not_stop_the_task() {
        let path = marker("survives");
        let (tx, cancel) = start(vec![
            hook(2, argv(&["/nonexistent/km-hub-hook"])),
            hook(2, argv(&["false"])), // spawns fine, exits non-zero
            hook(3, argv(&["touch", &path.to_string_lossy()])),
        ])
        .await;
        tx.send(HookEvent::Slot(2)).await.unwrap();
        tx.send(HookEvent::Slot(3)).await.unwrap();
        assert!(wait_for(&path).await, "task died on the failing hooks");
        cancel.cancel();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_hook_that_overruns_its_timeout_is_killed() {
        let path = marker("timeout");
        let mut slow = hook(2, argv(&["sleep", "60"]));
        slow.timeout = Duration::from_millis(200);
        let (tx, cancel) = start(vec![slow, hook(3, argv(&["touch", &path.to_string_lossy()]))]).await;
        tx.send(HookEvent::Slot(2)).await.unwrap();
        // The killed hook must not hold the task: a later switch still fires.
        tx.send(HookEvent::Slot(3)).await.unwrap();
        assert!(wait_for(&path).await, "task was blocked by the slow hook");
        cancel.cancel();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_macro_runs_by_index_and_hooks_stay_separate() {
        let (one, two, hook_path) = (marker("macro-1"), marker("macro-2"), marker("macro-hook"));
        let (tx, cancel) = start_with(
            vec![hook(2, argv(&["touch", &hook_path.to_string_lossy()]))],
            vec![
                a_macro("ctrl+alt+kp1", argv(&["touch", &one.to_string_lossy()])),
                a_macro("ctrl+alt+kp2", argv(&["touch", &two.to_string_lossy()])),
            ],
        )
        .await;
        tx.send(HookEvent::Macro(1)).await.unwrap();
        assert!(wait_for(&two).await, "macro 1 did not run");
        assert!(!one.exists(), "macro 0 ran for a macro 1 event");
        assert!(!hook_path.exists(), "a slot hook ran for a macro event");
        // Out of range is logged and ignored; the task keeps serving.
        tx.send(HookEvent::Macro(7)).await.unwrap();
        tx.send(HookEvent::Macro(0)).await.unwrap();
        assert!(wait_for(&one).await, "macro 0 did not run after a bad index");
        cancel.cancel();
        for p in [one, two, hook_path] {
            let _ = std::fs::remove_file(p);
        }
    }
}
