use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::warn;

use crate::input::keys::KeyCombo;
use crate::rgb::Rgb;

/// Slot 1 is always the local host (Ctrl+Alt+F1).
pub const LOCAL_SLOT: u8 = 1;

/// Highest hotkey slot (Ctrl+Alt+F12).
pub const MAX_SLOT: u8 = 12;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_alias")]
    adapter_alias: String,
    /// Explicit input device paths; `None` means auto-detect.
    devices: Option<Vec<PathBuf>>,
    /// Max mouse *motion* report rate over Bluetooth (Hz). Buttons and keys
    /// are never delayed. Gaming mice poll at 500–1000 Hz, which floods a BT
    /// link and shows up as cursor lag; BT mice normally report ~125 Hz.
    #[serde(default = "default_mouse_rate_hz")]
    mouse_rate_hz: u32,
    /// Slot lighting through a local OpenRGB server. Absent means off.
    rgb: Option<RawRgb>,
    #[serde(rename = "target", default)]
    targets: Vec<RawTarget>,
    /// Commands to run when a slot becomes the active target. Empty means the
    /// hook task never starts.
    #[serde(rename = "hook", default)]
    hooks: Vec<RawHook>,
    /// Commands run on a key combo, on any slot. Empty means nothing extra.
    #[serde(rename = "macro", default)]
    macros: Vec<RawMacro>,
}

fn default_mouse_rate_hz() -> u32 {
    125
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    name: String,
    slot: u8,
    /// Optional pre-seed; without it the slot is bound via a pairing window.
    mac: Option<String>,
    /// Lighting for this slot; falls back to the `[rgb]` palette.
    color: Option<String>,
    /// Lighting brightness for this slot; falls back to `[rgb] brightness`.
    brightness: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHook {
    slot: u8,
    run: RawRun,
    /// Shown in log lines; defaults to "slot <n>".
    name: Option<String>,
    #[serde(default = "default_hook_timeout_secs")]
    timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMacro {
    /// e.g. "ctrl+alt+kp1"; see `input::keys`.
    keys: String,
    run: RawRun,
    /// Shown in log lines; defaults to the canonical combo string.
    name: Option<String>,
    #[serde(default = "default_hook_timeout_secs")]
    timeout_secs: u64,
}

/// A bare string goes through `sh -c`, so `$VARS`, pipes and redirection work.
/// An array is exec'd directly, so nothing is interpreted — pick per hook.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRun {
    Shell(String),
    Argv(Vec<String>),
}

/// Shared by hooks and macros: reject an empty command, clamp the timeout.
fn resolve_run(what: &str, label: &str, run: RawRun, timeout_secs: u64) -> Result<(HookCommand, Duration)> {
    let command = match run {
        RawRun::Shell(line) if line.trim().is_empty() => {
            bail!("{what} '{label}': run is empty")
        }
        RawRun::Shell(line) => HookCommand::Shell(line),
        RawRun::Argv(argv) => {
            // An empty program would hand execvp("") to the kernel and
            // surface as a puzzling ENOENT at switch time instead.
            if argv.first().is_none_or(|program| program.is_empty()) {
                bail!("{what} '{label}': run is empty");
            }
            HookCommand::Argv(argv)
        }
    };
    Ok((command, Duration::from_secs(timeout_secs.clamp(1, 300))))
}

fn default_hook_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRgb {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_rgb_server")]
    server: String,
    /// 0-100, mapped onto each device mode's own brightness range.
    #[serde(default = "default_brightness")]
    brightness: u8,
    #[serde(default = "default_local_color")]
    local_color: String,
    #[serde(default = "default_pairing_color")]
    pairing_color: String,
    #[serde(default = "default_bound_color")]
    bound_color: String,
    /// Colors for slots that no `[[target]]` gives one, in slot order.
    #[serde(default = "default_palette")]
    palette: Vec<String>,
    /// Seconds without keyboard or mouse input before the LEDs fade out; 0
    /// keeps them lit forever.
    #[serde(default = "default_idle_timeout_secs")]
    idle_timeout_secs: u64,
}

fn default_idle_timeout_secs() -> u64 {
    120
}

fn default_rgb_server() -> String {
    "127.0.0.1:6742".into()
}

fn default_brightness() -> u8 {
    100
}

fn default_local_color() -> String {
    "#ffffff".into()
}

fn default_pairing_color() -> String {
    "#ffa000".into()
}

fn default_bound_color() -> String {
    "#00ff00".into()
}

fn default_palette() -> Vec<String> {
    ["#ff0000", "#0000ff", "#00ff00", "#ff00ff", "#00ffff", "#ff8000"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn default_alias() -> String {
    "KMHub".into()
}

#[derive(Debug, Clone)]
pub struct RemoteTarget {
    pub name: String,
    pub slot: u8,
    /// `None` means the slot's device is learned via a pairing window.
    pub addr: Option<bluer::Address>,
    pub color: Option<Rgb>,
    pub brightness: Option<u8>,
}

/// What a hook runs. The two forms differ only in whether a shell interprets
/// the command line; see [`RawRun`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCommand {
    Shell(String),
    /// Non-empty, and `argv[0]` is non-empty — both enforced at parse time.
    Argv(Vec<String>),
}

/// A command to run when `slot` becomes the active target.
#[derive(Debug, Clone)]
pub struct Hook {
    pub slot: u8,
    /// For log lines only.
    pub label: String,
    pub command: HookCommand,
    pub timeout: Duration,
}

/// A command to run when `combo` is pressed, whatever the active slot.
#[derive(Debug, Clone)]
pub struct Macro {
    pub combo: KeyCombo,
    /// For log lines only.
    pub label: String,
    pub command: HookCommand,
    pub timeout: Duration,
}

/// Resolved lighting settings. Colors are looked up per slot, so a slot bound
/// through a pairing window (no `[[target]]` entry at all) still gets one.
#[derive(Debug, Clone)]
pub struct RgbSettings {
    pub enabled: bool,
    pub server: String,
    pub brightness: u8,
    pub local_color: Rgb,
    pub pairing_color: Rgb,
    pub bound_color: Rgb,
    /// How long the hub's own input devices may stay quiet before the LEDs
    /// fade out. `None` disables blanking entirely.
    pub idle_timeout: Option<Duration>,
    palette: Vec<Rgb>,
    /// Per-slot overrides from `[[target]]` entries.
    slots: BTreeMap<u8, (Option<Rgb>, Option<u8>)>,
}

impl RgbSettings {
    fn disabled() -> Self {
        Self {
            enabled: false,
            server: default_rgb_server(),
            brightness: default_brightness(),
            local_color: Rgb::OFF,
            pairing_color: Rgb::OFF,
            bound_color: Rgb::OFF,
            idle_timeout: None,
            palette: Vec::new(),
            slots: BTreeMap::new(),
        }
    }

    fn parse(raw: RawRgb, targets: &[RemoteTarget]) -> Result<Self> {
        let color = |text: &str, field: &str| {
            Rgb::parse(text).with_context(|| format!("[rgb] {field}"))
        };
        if raw.palette.is_empty() {
            bail!("[rgb] palette must list at least one color");
        }
        let palette = raw
            .palette
            .iter()
            .map(|text| color(text, "palette"))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            enabled: raw.enabled,
            server: raw.server,
            brightness: raw.brightness.min(100),
            local_color: color(&raw.local_color, "local_color")?,
            pairing_color: color(&raw.pairing_color, "pairing_color")?,
            bound_color: color(&raw.bound_color, "bound_color")?,
            // 0 is the documented "never blank" switch, not a zero-length
            // timeout that would fade out the instant you stop typing.
            idle_timeout: (raw.idle_timeout_secs > 0)
                .then(|| Duration::from_secs(raw.idle_timeout_secs)),
            palette,
            slots: targets
                .iter()
                .map(|t| (t.slot, (t.color, t.brightness.map(|b| b.min(100)))))
                .collect(),
        })
    }

    pub fn slot_color(&self, slot: u8) -> Rgb {
        if slot == LOCAL_SLOT {
            return self.local_color;
        }
        if let Some(color) = self.slots.get(&slot).and_then(|(color, _)| *color) {
            return color;
        }
        if self.palette.is_empty() {
            return self.local_color;
        }
        // Slot 2 takes the first palette entry, and it wraps from there.
        let index = usize::from(slot.saturating_sub(LOCAL_SLOT + 1)) % self.palette.len();
        self.palette[index]
    }

    pub fn slot_brightness(&self, slot: u8) -> u8 {
        self.slots
            .get(&slot)
            .and_then(|(_, brightness)| *brightness)
            .unwrap_or(self.brightness)
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::parse(
            RawRgb {
                enabled: true,
                server: default_rgb_server(),
                brightness: 60,
                local_color: default_local_color(),
                pairing_color: default_pairing_color(),
                bound_color: default_bound_color(),
                palette: default_palette(),
                idle_timeout_secs: default_idle_timeout_secs(),
            },
            &[],
        )
        .expect("built-in defaults are valid")
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub adapter_alias: String,
    pub devices: Option<Vec<PathBuf>>,
    /// See `RawConfig::mouse_rate_hz`.
    pub mouse_rate_hz: u32,
    pub rgb: RgbSettings,
    pub targets: Vec<RemoteTarget>,
    pub hooks: Vec<Hook>,
    /// In file order; the hotkey FSM reports a macro by its index here.
    pub macros: Vec<Macro>,
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read config {} (copy config.example.toml to config.toml to get started)",
                path.display()
            )
        })?;
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)?;

        let mut targets = Vec::new();
        for t in raw.targets {
            if t.slot <= LOCAL_SLOT || t.slot > MAX_SLOT {
                bail!(
                    "target '{}': slot must be {}..={MAX_SLOT} (slot {LOCAL_SLOT} is the local host)",
                    t.name,
                    LOCAL_SLOT + 1,
                );
            }
            if targets.iter().any(|o: &RemoteTarget| o.slot == t.slot) {
                bail!("duplicate target slot {}", t.slot);
            }
            let addr = t
                .mac
                .as_deref()
                .map(|mac| {
                    mac.parse::<bluer::Address>()
                        .with_context(|| format!("target '{}': invalid MAC '{mac}'", t.name))
                })
                .transpose()?;
            let color = t
                .color
                .as_deref()
                .map(|color| {
                    Rgb::parse(color).with_context(|| format!("target '{}'", t.name))
                })
                .transpose()?;
            targets.push(RemoteTarget {
                name: t.name,
                slot: t.slot,
                addr,
                color,
                brightness: t.brightness,
            });
        }

        let rgb = match raw.rgb {
            Some(rgb) => RgbSettings::parse(rgb, &targets)?,
            None => RgbSettings::disabled(),
        };

        let mut hooks = Vec::new();
        for h in raw.hooks {
            let label = h.name.unwrap_or_else(|| format!("slot {}", h.slot));
            // Unlike [[target]], the local slot is hookable: "I'm back on the
            // hub" is exactly the kind of thing worth acting on.
            if h.slot < LOCAL_SLOT || h.slot > MAX_SLOT {
                bail!("hook '{label}': slot must be {LOCAL_SLOT}..={MAX_SLOT}");
            }
            let (command, timeout) = resolve_run("hook", &label, h.run, h.timeout_secs)?;
            hooks.push(Hook {
                slot: h.slot,
                label,
                command,
                timeout,
            });
        }

        let mut macros: Vec<Macro> = Vec::new();
        for m in raw.macros {
            let combo: KeyCombo = m
                .keys
                .parse()
                .with_context(|| format!("macro '{}': invalid keys", m.name.as_deref().unwrap_or(&m.keys)))?;
            let label = m.name.unwrap_or_else(|| combo.to_string());
            if combo.is_reserved() {
                bail!(
                    "macro '{label}': {combo} is reserved — Ctrl+Alt+F1..F12 (with any extra \
                     modifier) switch slots"
                );
            }
            if let Some(other) = macros.iter().find(|o| o.combo == combo) {
                bail!("macro '{label}': {combo} is already bound by macro '{}'", other.label);
            }
            if combo.mods.is_empty() {
                // Legal (dedicated macro keys exist), but a bare letter is more
                // likely a typo — and it will be swallowed on every target.
                warn!(
                    "macro '{label}': '{combo}' has no modifier — that key alone will run the \
                     macro and never reach any target"
                );
            }
            let (command, timeout) = resolve_run("macro", &label, m.run, m.timeout_secs)?;
            macros.push(Macro {
                combo,
                label,
                command,
                timeout,
            });
        }

        Ok(Self {
            adapter_alias: raw.adapter_alias,
            mouse_rate_hz: raw.mouse_rate_hz.clamp(20, 1000),
            devices: raw.devices,
            rgb,
            targets,
            hooks,
            macros,
        })
    }

    pub fn target_for_slot(&self, slot: u8) -> Option<&RemoteTarget> {
        self.targets.iter().find(|t| t.slot == slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: &str) -> Rgb {
        Rgb::parse(hex).unwrap()
    }

    #[test]
    fn lighting_is_off_unless_configured() {
        let settings = Settings::parse("adapter_alias = \"hub\"").unwrap();
        assert!(!settings.rgb.enabled);
    }

    /// The idle timeout is on by default; 0 is the documented way to say
    /// "never blank", not a zero-length timeout that blanks instantly.
    #[test]
    fn a_zero_idle_timeout_disables_blanking() {
        let on = Settings::parse("[rgb]\nenabled = true").unwrap();
        assert_eq!(on.rgb.idle_timeout, Some(Duration::from_secs(120)));

        let tuned = Settings::parse("[rgb]\nenabled = true\nidle_timeout_secs = 300").unwrap();
        assert_eq!(tuned.rgb.idle_timeout, Some(Duration::from_secs(300)));

        let never = Settings::parse("[rgb]\nenabled = true\nidle_timeout_secs = 0").unwrap();
        assert_eq!(never.rgb.idle_timeout, None);
    }

    #[test]
    fn slot_colors_come_from_targets_then_the_palette() {
        let settings = Settings::parse(
            r##"
            [rgb]
            enabled = true
            brightness = 60
            local_color = "#ffffff"
            palette = ["#ff0000", "#0000ff"]

            [[target]]
            name = "tower"
            slot = 2
            color = "#123456"

            [[target]]
            name = "macbook"
            slot = 3
            "##,
        )
        .unwrap();
        let lighting = &settings.rgb;
        assert!(lighting.enabled);
        assert_eq!(lighting.server, "127.0.0.1:6742");
        // Slot 1 is the hub itself.
        assert_eq!(lighting.slot_color(LOCAL_SLOT), rgb("#ffffff"));
        // An explicit target color wins.
        assert_eq!(lighting.slot_color(2), rgb("#123456"));
        // A target without one falls through to the palette, which starts at
        // slot 2 and wraps.
        assert_eq!(lighting.slot_color(3), rgb("#0000ff"));
        assert_eq!(lighting.slot_color(4), rgb("#ff0000"));
        // A slot with no target entry at all (bound via a pairing window).
        assert_eq!(lighting.slot_color(5), rgb("#0000ff"));
    }

    #[test]
    fn brightness_falls_back_to_the_global_value() {
        let settings = Settings::parse(
            r##"
            [rgb]
            enabled = true
            brightness = 60

            [[target]]
            name = "tower"
            slot = 2
            brightness = 90

            [[target]]
            name = "macbook"
            slot = 3
            "##,
        )
        .unwrap();
        assert_eq!(settings.rgb.slot_brightness(2), 90);
        assert_eq!(settings.rgb.slot_brightness(3), 60);
        assert_eq!(settings.rgb.slot_brightness(LOCAL_SLOT), 60);
    }

    #[test]
    fn out_of_range_brightness_is_clamped() {
        let settings = Settings::parse(
            r##"
            [rgb]
            enabled = true
            brightness = 200

            [[target]]
            name = "tower"
            slot = 2
            brightness = 180
            "##,
        )
        .unwrap();
        assert_eq!(settings.rgb.brightness, 100);
        assert_eq!(settings.rgb.slot_brightness(2), 100);
    }

    #[test]
    fn bad_colors_name_the_offending_entry() {
        let err = Settings::parse(
            r##"
            [rgb]
            enabled = true

            [[target]]
            name = "tower"
            slot = 2
            color = "reddish"
            "##,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("tower"), "{message}");
        assert!(message.contains("reddish"), "{message}");

        let err = Settings::parse("[rgb]\nenabled = true\nlocal_color = \"#xyzxyz\"").unwrap_err();
        assert!(format!("{err:#}").contains("local_color"));
    }

    #[test]
    fn an_empty_palette_is_rejected() {
        let err = Settings::parse("[rgb]\nenabled = true\npalette = []").unwrap_err();
        assert!(format!("{err:#}").contains("palette"));
    }

    #[test]
    fn defaults_apply_when_only_enabled_is_set() {
        let settings = Settings::parse("[rgb]\nenabled = true").unwrap();
        let lighting = &settings.rgb;
        assert_eq!(lighting.brightness, 100);
        assert_eq!(lighting.local_color, rgb("#ffffff"));
        assert_eq!(lighting.pairing_color, rgb("#ffa000"));
        assert_eq!(lighting.bound_color, rgb("#00ff00"));
        assert_eq!(lighting.slot_color(2), rgb("#ff0000"));
        assert_eq!(lighting.slot_color(3), rgb("#0000ff"));
    }

    /// The shipped example is what users copy; a typo in it is a real bug.
    #[test]
    fn the_example_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.toml");
        let settings = Settings::load(Path::new(path)).unwrap();
        assert!(settings.rgb.enabled);
        assert_eq!(settings.rgb.brightness, 60);
        assert_eq!(settings.rgb.slot_color(2), rgb("#ff0000"));
    }

    #[test]
    fn unknown_rgb_keys_are_rejected() {
        assert!(Settings::parse("[rgb]\nenabled = true\ncolour = \"#fff\"").is_err());
    }

    #[test]
    fn hooks_are_absent_by_default() {
        assert!(Settings::parse("adapter_alias = \"hub\"").unwrap().hooks.is_empty());
    }

    #[test]
    fn a_hook_takes_a_shell_string_or_an_argv_array() {
        let settings = Settings::parse(
            r#"
            [[hook]]
            slot = 2
            run = "curl -fsS http://ha/x"

            [[hook]]
            slot = 3
            name = "notify"
            run = ["/usr/local/bin/notify", "macbook"]
            "#,
        )
        .unwrap();
        assert_eq!(
            settings.hooks[0].command,
            HookCommand::Shell("curl -fsS http://ha/x".into())
        );
        // Without a name, log lines fall back to the slot.
        assert_eq!(settings.hooks[0].label, "slot 2");
        assert_eq!(
            settings.hooks[1].command,
            HookCommand::Argv(vec!["/usr/local/bin/notify".into(), "macbook".into()])
        );
        assert_eq!(settings.hooks[1].label, "notify");
    }

    /// Unlike `[[target]]`, which starts at slot 2, a hook may fire for the hub
    /// itself — "I'm back on the local machine" is worth acting on.
    #[test]
    fn a_hook_may_target_the_local_slot() {
        let settings = Settings::parse("[[hook]]\nslot = 1\nrun = \"true\"").unwrap();
        assert_eq!(settings.hooks[0].slot, LOCAL_SLOT);
        assert!(Settings::parse("[[target]]\nname = \"x\"\nslot = 1").is_err());
    }

    #[test]
    fn several_hooks_may_share_a_slot() {
        let settings = Settings::parse(
            r#"
            [[hook]]
            slot = 2
            run = "first"

            [[hook]]
            slot = 2
            run = "second"
            "#,
        )
        .unwrap();
        assert_eq!(settings.hooks.len(), 2);
        assert!(settings.hooks.iter().all(|h| h.slot == 2));
    }

    #[test]
    fn an_empty_run_is_rejected() {
        for run in ["\"\"", "\"   \"", "[]", "[\"\"]"] {
            let err = Settings::parse(&format!("[[hook]]\nslot = 2\nrun = {run}")).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("empty"), "{run}: {message}");
        }
    }

    #[test]
    fn an_out_of_range_hook_slot_is_rejected() {
        for slot in [0, 13] {
            let err =
                Settings::parse(&format!("[[hook]]\nslot = {slot}\nrun = \"true\"")).unwrap_err();
            assert!(format!("{err:#}").contains("slot"), "slot {slot}");
        }
    }

    /// The offending hook must be identifiable, since a config may hold many.
    #[test]
    fn a_bad_hook_names_itself() {
        let err =
            Settings::parse("[[hook]]\nslot = 99\nname = \"desk lamp\"\nrun = \"true\"").unwrap_err();
        assert!(format!("{err:#}").contains("desk lamp"));
    }

    #[test]
    fn hook_timeouts_default_and_clamp() {
        let settings = Settings::parse(
            r#"
            [[hook]]
            slot = 2
            run = "true"

            [[hook]]
            slot = 3
            run = "true"
            timeout_secs = 0

            [[hook]]
            slot = 4
            run = "true"
            timeout_secs = 9999
            "#,
        )
        .unwrap();
        assert_eq!(settings.hooks[0].timeout, Duration::from_secs(10));
        assert_eq!(settings.hooks[1].timeout, Duration::from_secs(1));
        assert_eq!(settings.hooks[2].timeout, Duration::from_secs(300));
    }

    #[test]
    fn unknown_hook_keys_are_rejected() {
        assert!(Settings::parse("[[hook]]\nslot = 2\nrun = \"true\"\non = \"activated\"").is_err());
    }

    #[test]
    fn macros_are_absent_by_default() {
        assert!(Settings::parse("adapter_alias = \"hub\"").unwrap().macros.is_empty());
    }

    #[test]
    fn a_macro_takes_keys_and_a_shell_string_or_argv_array() {
        let settings = Settings::parse(
            r#"
            [[macro]]
            keys = "Ctrl+Alt+KP1"
            run = "curl -fsS http://ha/x"

            [[macro]]
            keys = "ctrl+alt+shift+kp1"
            name = "lamp"
            run = ["/usr/local/bin/lamp", "on"]
            timeout_secs = 0
            "#,
        )
        .unwrap();
        assert_eq!(settings.macros.len(), 2);
        assert_eq!(settings.macros[0].combo, "ctrl+alt+kp1".parse().unwrap());
        assert_eq!(settings.macros[0].command, HookCommand::Shell("curl -fsS http://ha/x".into()));
        // Without a name, log lines fall back to the canonical combo.
        assert_eq!(settings.macros[0].label, "ctrl+alt+kp1");
        assert_eq!(settings.macros[0].timeout, Duration::from_secs(10));
        assert_eq!(settings.macros[1].label, "lamp");
        assert_eq!(
            settings.macros[1].command,
            HookCommand::Argv(vec!["/usr/local/bin/lamp".into(), "on".into()])
        );
        assert_eq!(settings.macros[1].timeout, Duration::from_secs(1));
    }

    #[test]
    fn a_macro_with_unknown_keys_names_the_token() {
        let err = Settings::parse("[[macro]]\nkeys = \"ctrl+bogus\"\nrun = \"true\"").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("bogus"), "{message}");
        assert!(Settings::parse("[[macro]]\nkeys = \"ctrl+alt\"\nrun = \"true\"").is_err());
        assert!(Settings::parse("[[macro]]\nkeys = \"ctrl+a+b\"\nrun = \"true\"").is_err());
    }

    #[test]
    fn a_macro_on_a_switch_combo_is_rejected() {
        for keys in ["ctrl+alt+f2", "alt+ctrl+shift+f12", "ctrl+alt+super+f1"] {
            let err = Settings::parse(&format!("[[macro]]\nkeys = \"{keys}\"\nrun = \"true\""))
                .unwrap_err();
            assert!(format!("{err:#}").contains("reserved"), "{keys}");
        }
        // Neighbours are fine.
        assert!(Settings::parse("[[macro]]\nkeys = \"ctrl+alt+f13\"\nrun = \"true\"").is_ok());
        assert!(Settings::parse("[[macro]]\nkeys = \"ctrl+shift+f2\"\nrun = \"true\"").is_ok());
    }

    #[test]
    fn duplicate_macro_combos_are_rejected_across_spellings() {
        let err = Settings::parse(
            "[[macro]]\nkeys = \"ctrl+alt+kp1\"\nname = \"first\"\nrun = \"true\"\n\
             [[macro]]\nkeys = \"Alt+Ctrl+KP1\"\nrun = \"true\"",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("first"));
        // Differing only by a modifier is a different combo.
        assert!(
            Settings::parse(
                "[[macro]]\nkeys = \"ctrl+alt+kp1\"\nrun = \"true\"\n\
                 [[macro]]\nkeys = \"ctrl+alt+shift+kp1\"\nrun = \"true\""
            )
            .is_ok()
        );
    }

    #[test]
    fn a_bare_key_macro_is_accepted() {
        let settings = Settings::parse("[[macro]]\nkeys = \"prog1\"\nrun = \"true\"").unwrap();
        assert!(settings.macros[0].combo.mods.is_empty());
    }

    #[test]
    fn an_empty_macro_run_is_rejected() {
        for run in ["\"\"", "[]", "[\"\"]"] {
            let err = Settings::parse(&format!("[[macro]]\nkeys = \"ctrl+alt+kp1\"\nrun = {run}"))
                .unwrap_err();
            assert!(format!("{err:#}").contains("empty"), "{run}");
        }
    }

    #[test]
    fn unknown_macro_keys_are_rejected() {
        assert!(Settings::parse("[[macro]]\nkeys = \"ctrl+alt+kp1\"\nrun = \"true\"\nslot = 2").is_err());
    }
}
