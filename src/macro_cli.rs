//! `km-hub macro …`: the small local tool that turns a keypress into the
//! `keys = "…"` string a `[[macro]]` needs, and optionally writes the entry.
//!
//!   km-hub macro capture            print the name of the next combo pressed
//!   km-hub macro add [--config P]   capture a combo, prompt for the command,
//!                                   append (or replace) a [[macro]] in the config
//!
//! Where the keys are read from: km-hub grabs the physical keyboards
//! exclusively, so while the daemon runs nobody else sees them. What *is*
//! visible then is the daemon's own uinput keyboard (`km-hub virtual
//! keyboard`), which carries everything forwarded to the local slot. So: if a
//! virtual keyboard exists we listen there (the local slot must be active);
//! otherwise we read the physical keyboards directly, without grabbing.
//! Combos the daemon already claims — switch combos, existing macros — are
//! swallowed before they reach the virtual device and can never be captured
//! while it runs; that is correct, they are taken.
//!
//! The config is edited with `toml_edit`, so comments and layout survive, and
//! the result is parsed with the real config parser before it is written.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use evdev::{Device, EventSummary, KeyCode};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::config::{HookCommand, Settings};
use crate::input::keys::{KeyCombo, Mods, modifier_of};
use crate::input::{VIRTUAL_PREFIX, is_keyboard};

const DEFAULT_TIMEOUT_SECS: u64 = 10;

pub async fn run(args: Vec<String>) -> Result<()> {
    let mut config = PathBuf::from("config.toml");
    let mut command: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => config = it.next().context("--config needs a path")?.into(),
            "--help" | "-h" => {
                println!("{}", USAGE.trim_end());
                return Ok(());
            }
            "capture" | "add" if command.is_none() => command = Some(arg),
            other => bail!("unknown argument '{other}' (see km-hub macro --help)"),
        }
    }
    let Some(command) = command else {
        bail!("usage: km-hub macro capture|add [--config <path>]");
    };

    // Polling `ctrl_c` once replaces the default SIGINT handler for good, so
    // keep one waiter for the whole run and let it end the process outright:
    // there is nothing to clean up, and a blocking stdin prompt must not be
    // able to swallow the interrupt.
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!();
        std::process::exit(130);
    });

    match command.as_str() {
        "capture" => capture().await,
        "add" => add(&config).await,
        _ => unreachable!(),
    }
}

const USAGE: &str = "\
usage: km-hub macro capture
       km-hub macro add [--config <path>]

capture   wait for one key combo and print its name, e.g. ctrl+alt+kp1
add       capture a combo, ask for a name, the command and a timeout, then
          append a [[macro]] entry to the config (default: ./config.toml).
          A combo that is already bound can be replaced in place.

Run this on the hub. If km-hub is running, its virtual keyboard is read, so
the LOCAL slot (Ctrl+Alt+F1) must be active; combos km-hub already claims are
swallowed before we see them. If km-hub is stopped, the physical keyboards are
read directly. Config changes take effect after km-hub restarts.
";

async fn capture() -> Result<()> {
    let combo = capture_combo().await?;
    println!("{combo}");
    if combo.is_reserved() {
        eprintln!("note: {combo} is reserved for slot switching and cannot be a macro");
    }
    Ok(())
}

async fn add(config: &Path) -> Result<()> {
    let text = std::fs::read_to_string(config)
        .with_context(|| format!("cannot read config {}", config.display()))?;
    // Fail on a config the daemon would refuse before asking anything.
    Settings::parse(&text).with_context(|| format!("invalid config {}", config.display()))?;
    let mut doc: DocumentMut = text.parse().context("cannot parse config as TOML")?;

    let (combo, replace) = loop {
        let combo = capture_combo().await?;
        println!("captured: {combo}");
        if combo.is_reserved() {
            eprintln!("{combo} is reserved for slot switching (Ctrl+Alt+F1..F12) — try another");
            continue;
        }
        match existing_macro(&doc, combo)? {
            Some(label) => {
                let answer = prompt(&format!("{combo} is already bound by '{label}' — replace it? [y/N] "))?;
                if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
                    break (combo, true);
                }
                println!("keeping '{label}' — press another combo");
            }
            None => break (combo, false),
        }
    };
    if combo.mods.is_empty() {
        println!(
            "note: {combo} has no modifier — that key alone will run the macro and never reach \
             any target"
        );
    }

    let name = prompt("name (optional, for log lines): ")?;
    let name = (!name.is_empty()).then_some(name);
    let line = loop {
        let line = prompt("command: ")?;
        if !line.is_empty() {
            break line;
        }
        println!("the command cannot be empty");
    };
    let command = loop {
        let shell = prompt("run through a shell (sh -c: $VARS, pipes, redirection)? [Y/n] ")?;
        if shell.is_empty() || shell.eq_ignore_ascii_case("y") || shell.eq_ignore_ascii_case("yes") {
            break HookCommand::Shell(line);
        }
        if shell.eq_ignore_ascii_case("n") || shell.eq_ignore_ascii_case("no") {
            let argv: Vec<String> = line.split_whitespace().map(str::to_string).collect();
            println!("exec'd directly, split on whitespace: {argv:?}");
            let ok = prompt("ok? [Y/n] ")?;
            if ok.is_empty() || ok.eq_ignore_ascii_case("y") || ok.eq_ignore_ascii_case("yes") {
                break HookCommand::Argv(argv);
            }
            println!("edit config.toml by hand for arguments containing spaces");
            break HookCommand::Argv(argv);
        }
    };
    let timeout_secs = loop {
        let raw = prompt(&format!("timeout in seconds [{DEFAULT_TIMEOUT_SECS}]: "))?;
        if raw.is_empty() {
            break DEFAULT_TIMEOUT_SECS;
        }
        match raw.parse::<u64>() {
            Ok(secs) if (1..=300).contains(&secs) => break secs,
            _ => println!("enter a whole number of seconds, 1..=300"),
        }
    };

    let entry = MacroEntry {
        combo,
        name,
        command,
        timeout_secs,
    };
    upsert_macro(&mut doc, &entry, replace)?;
    let new_text = doc.to_string();
    Settings::parse(&new_text).context("the edited config does not parse — nothing written")?;

    println!("\n{}", render_entry(&entry).trim_end());
    write_atomically(config, &new_text)?;
    println!(
        "\n{} {} — restart km-hub to apply (sudo systemctl restart km-hub, or `make restart`).\n\
         If you deploy from another machine, run `make pull-config` there before the next \
         `make sync`, or the copy there overwrites this one.",
        if replace { "replaced in" } else { "written to" },
        config.display()
    );
    Ok(())
}

struct MacroEntry {
    combo: KeyCombo,
    name: Option<String>,
    command: HookCommand,
    timeout_secs: u64,
}

/// The `[[macro]]` tables of the document, created empty if absent. Errors if
/// `macro` exists but is not an array of tables (`macro = [{...}]` inline).
fn macro_tables(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    doc.as_table_mut()
        .entry("macro")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .context("'macro' in the config is not a [[macro]] table array — edit it by hand")
}

/// Label of the macro already bound to `combo`, comparing canonical forms so
/// `Alt+Ctrl+KP1` and `ctrl+alt+kp1` are the same binding.
fn existing_macro(doc: &DocumentMut, combo: KeyCombo) -> Result<Option<String>> {
    let Some(tables) = doc.get("macro").and_then(Item::as_array_of_tables) else {
        return Ok(None);
    };
    for table in tables.iter() {
        let keys = table.get("keys").and_then(Item::as_str).unwrap_or_default();
        // The config was validated above, so this parses; be lenient anyway.
        if keys.parse::<KeyCombo>().ok() == Some(combo) {
            let label = table
                .get("name")
                .and_then(Item::as_str)
                .map_or_else(|| combo.to_string(), str::to_string);
            return Ok(Some(label));
        }
    }
    Ok(None)
}

fn run_item(command: &HookCommand) -> Item {
    match command {
        HookCommand::Shell(line) => value(line.as_str()),
        HookCommand::Argv(argv) => {
            let mut arr = Array::new();
            for arg in argv {
                arr.push(arg.as_str());
            }
            value(arr)
        }
    }
}

/// Fill `table` from `entry`, dropping optional keys that are at their default
/// so a replaced entry does not keep a stale name or timeout.
fn fill_table(table: &mut Table, entry: &MacroEntry) {
    table.insert("keys", value(entry.combo.to_string()));
    match &entry.name {
        Some(name) => {
            table.insert("name", value(name.as_str()));
        }
        None => {
            table.remove("name");
        }
    }
    table.insert("run", run_item(&entry.command));
    if entry.timeout_secs == DEFAULT_TIMEOUT_SECS {
        table.remove("timeout_secs");
    } else {
        table.insert("timeout_secs", value(entry.timeout_secs as i64));
    }
}

/// Append `entry` as a new `[[macro]]`, or with `replace` overwrite the table
/// already bound to the same combo. Everything else in the document — comments,
/// ordering, other tables — is left as it was.
fn upsert_macro(doc: &mut DocumentMut, entry: &MacroEntry, replace: bool) -> Result<()> {
    let tables = macro_tables(doc)?;
    let existing = tables.iter_mut().find(|t| {
        t.get("keys")
            .and_then(Item::as_str)
            .and_then(|k| k.parse::<KeyCombo>().ok())
            == Some(entry.combo)
    });
    match (existing, replace) {
        (Some(table), true) => fill_table(table, entry),
        (Some(_), false) => bail!("{} is already bound", entry.combo),
        (None, _) => {
            let mut table = Table::new();
            fill_table(&mut table, entry);
            tables.push(table);
        }
    }
    Ok(())
}

/// The entry as it will look in the file, for the confirmation printout.
fn render_entry(entry: &MacroEntry) -> String {
    let mut doc = DocumentMut::new();
    let mut table = Table::new();
    fill_table(&mut table, entry);
    let mut tables = ArrayOfTables::new();
    tables.push(table);
    doc.insert("macro", Item::ArrayOfTables(tables));
    doc.to_string()
}

fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("config.toml")
    ));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

fn prompt(text: &str) -> Result<String> {
    print!("{text}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("stdin closed");
    }
    Ok(line.trim().to_string())
}

/// Wait for one combo: the first press of a non-modifier key completes it with
/// the modifiers held at that moment; then wait for everything to be released,
/// so the tail of the combo cannot leak into whatever runs next.
async fn capture_combo() -> Result<KeyCombo> {
    let (devices, virtual_kbd) = open_keyboards()?;
    if virtual_kbd {
        println!(
            "km-hub is running — reading its virtual keyboard: the LOCAL slot (Ctrl+Alt+F1) \
             must be active, and combos km-hub already uses cannot be captured"
        );
    } else {
        println!("km-hub is not running — reading {} keyboard(s) directly", devices.len());
    }
    println!("press the key combo… (Ctrl-C to abort)");

    // One reader per device into a channel; the tasks die with the JoinSet.
    let (tx, mut rx) = mpsc::channel::<(KeyCode, i32)>(64);
    let mut readers = JoinSet::new();
    for (path, dev) in devices {
        let mut stream = dev
            .into_event_stream()
            .with_context(|| format!("cannot stream {}", path.display()))?;
        let tx = tx.clone();
        readers.spawn(async move {
            while let Ok(event) = stream.next_event().await {
                if let EventSummary::Key(_, key, val) = event.destructure()
                    && tx.send((key, val)).await.is_err()
                {
                    break;
                }
            }
        });
    }
    drop(tx);

    // Only presses we saw count as held: whatever was down when we started
    // (the Enter that launched us) is somebody else's business.
    let mut held: HashSet<KeyCode> = HashSet::new();
    let mut combo: Option<KeyCombo> = None;
    while let Some((key, val)) = rx.recv().await {
        match val {
            1 => {
                held.insert(key);
                if combo.is_none() && modifier_of(key).is_none() {
                    combo = Some(KeyCombo {
                        mods: Mods::from_held(&held),
                        key,
                    });
                }
            }
            0 => {
                held.remove(&key);
            }
            _ => {}
        }
        if let Some(combo) = combo
            && held.is_empty()
        {
            return Ok(combo);
        }
    }
    bail!("all keyboards went away before a combo was pressed");
}

/// The keyboards to read and whether they are km-hub's virtual one (i.e. the
/// daemon is running and holds the physical ones).
fn open_keyboards() -> Result<(Vec<(PathBuf, Device)>, bool)> {
    let mut physical = Vec::new();
    let mut virt = Vec::new();
    for (path, dev) in evdev::enumerate() {
        if !is_keyboard(&dev) {
            continue;
        }
        if dev.name().unwrap_or("").starts_with(VIRTUAL_PREFIX) {
            virt.push((path, dev));
        } else {
            physical.push((path, dev));
        }
    }
    if !virt.is_empty() {
        return Ok((virt, true));
    }
    if physical.is_empty() {
        bail!(
            "no keyboard found — check permissions on /dev/input/event* \
             (run ./setup.sh, then log out and back in)"
        );
    }
    Ok((physical, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(keys: &str, name: Option<&str>, command: HookCommand, timeout_secs: u64) -> MacroEntry {
        MacroEntry {
            combo: keys.parse().unwrap(),
            name: name.map(str::to_string),
            command,
            timeout_secs,
        }
    }

    const CONFIG: &str = r#"# hub config
adapter_alias = "KMHub"   # keep me

[[hook]]
slot = 2
run = "true"

# first macro
[[macro]]
keys = "Alt+Ctrl+KP1"
name = "lamp"
run = "lamp on"
timeout_secs = 30
"#;

    #[test]
    fn append_preserves_everything_else_and_parses() {
        let mut doc: DocumentMut = CONFIG.parse().unwrap();
        let e = entry("ctrl+alt+kp2", None, HookCommand::Argv(vec!["/bin/x".into(), "a b".into()]), 10);
        upsert_macro(&mut doc, &e, false).unwrap();
        let out = doc.to_string();
        assert!(out.starts_with(CONFIG), "prefix changed:\n{out}");
        assert!(out.contains("keys = \"ctrl+alt+kp2\""));
        assert!(out.contains("run = [\"/bin/x\", \"a b\"]"));
        // Defaults are not written out.
        assert_eq!(out.matches("timeout_secs").count(), 1);
        assert!(!out.contains("name = \"\""));
        let settings = Settings::parse(&out).unwrap();
        assert_eq!(settings.macros.len(), 2);
        assert_eq!(settings.macros[1].command, e.command);
    }

    #[test]
    fn append_into_a_config_without_macros() {
        let mut doc: DocumentMut = "adapter_alias = \"x\"\n".parse().unwrap();
        upsert_macro(&mut doc, &entry("super+a", Some("n"), HookCommand::Shell("true".into()), 5), false)
            .unwrap();
        let out = doc.to_string();
        assert!(out.contains("[[macro]]"), "{out}");
        let settings = Settings::parse(&out).unwrap();
        assert_eq!(settings.macros[0].label, "n");
        assert_eq!(settings.macros[0].timeout, std::time::Duration::from_secs(5));
    }

    #[test]
    fn duplicates_are_detected_across_spellings_and_replaced_in_place() {
        let mut doc: DocumentMut = CONFIG.parse().unwrap();
        assert_eq!(existing_macro(&doc, "ctrl+alt+kp1".parse().unwrap()).unwrap().as_deref(), Some("lamp"));
        assert_eq!(existing_macro(&doc, "ctrl+alt+kp2".parse().unwrap()).unwrap(), None);
        let e = entry("ctrl+alt+kp1", None, HookCommand::Shell("lamp off".into()), 10);
        assert!(upsert_macro(&mut doc, &e, false).is_err());
        upsert_macro(&mut doc, &e, true).unwrap();
        let out = doc.to_string();
        // Still exactly one macro, comment kept, stale name/timeout gone.
        assert_eq!(out.matches("[[macro]]").count(), 1);
        assert!(out.contains("# first macro"));
        assert!(!out.contains("name = \"lamp\""));
        assert!(!out.contains("timeout_secs"));
        assert!(out.contains("run = \"lamp off\""));
        assert!(out.contains("keys = \"ctrl+alt+kp1\""));
        let settings = Settings::parse(&out).unwrap();
        assert_eq!(settings.macros.len(), 1);
        assert_eq!(settings.macros[0].label, "ctrl+alt+kp1");
    }

    #[test]
    fn an_inline_macro_array_is_refused() {
        let mut doc: DocumentMut = "macro = [{ keys = \"ctrl+a\", run = \"x\" }]\n".parse().unwrap();
        let e = entry("ctrl+b", None, HookCommand::Shell("y".into()), 10);
        assert!(upsert_macro(&mut doc, &e, false).is_err());
    }
}
