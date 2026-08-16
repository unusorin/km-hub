//! Learned slot bindings: which Bluetooth device each hotkey slot switches to.
//!
//! Bindings come from two sources: `state.toml` (learned via pairing windows,
//! persisted here) and `config.toml` targets with an explicit `mac` (pre-seeds,
//! which override the state file for their slot and are never written back).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use bluer::Address;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::RemoteTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub addr: Address,
    pub name: String,
    /// Seeded from config.toml; not persisted to the state file.
    pub from_config: bool,
}

/// slot -> bound device
pub type Bindings = BTreeMap<u8, Binding>;

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(rename = "binding", default)]
    bindings: Vec<StateBinding>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateBinding {
    slot: u8,
    mac: String,
    name: String,
}

pub fn load(path: &Path, targets: &[RemoteTarget]) -> Result<Bindings> {
    let file = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read state file {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("invalid state file {}", path.display()))?
    } else {
        StateFile::default()
    };
    Ok(merge(file, targets))
}

fn merge(file: StateFile, targets: &[RemoteTarget]) -> Bindings {
    let mut map = Bindings::new();
    for b in file.bindings {
        match b.mac.parse() {
            Ok(addr) => {
                map.insert(
                    b.slot,
                    Binding {
                        addr,
                        name: b.name,
                        from_config: false,
                    },
                );
            }
            Err(err) => warn!(slot = b.slot, mac = %b.mac, %err, "ignoring state entry with bad MAC"),
        }
    }
    // Config pre-seeds win over learned state for the same slot.
    for t in targets {
        if let Some(addr) = t.addr {
            map.insert(
                t.slot,
                Binding {
                    addr,
                    name: t.name.clone(),
                    from_config: true,
                },
            );
        }
    }
    map
}

/// Persist the learned bindings (config pre-seeds are excluded — config.toml
/// stays the source of truth for those). Failure is logged, never fatal.
pub fn save(path: &Path, bindings: &Bindings) {
    let text = match toml::to_string_pretty(&to_state_file(bindings)) {
        Ok(text) => text,
        Err(err) => {
            warn!(%err, "cannot serialize state");
            return;
        }
    };
    if let Err(err) = std::fs::write(path, text) {
        warn!(state = %path.display(), %err, "cannot write state file — binding will be lost on restart");
    }
}

fn to_state_file(bindings: &Bindings) -> StateFile {
    StateFile {
        bindings: bindings
            .iter()
            .filter(|(_, b)| !b.from_config)
            .map(|(&slot, b)| StateBinding {
                slot,
                mac: b.addr.to_string(),
                name: b.name.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8) -> Address {
        Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, last])
    }

    fn target(slot: u8, name: &str, mac: Option<u8>) -> RemoteTarget {
        RemoteTarget {
            name: name.into(),
            slot,
            addr: mac.map(addr),
            color: None,
            brightness: None,
        }
    }

    #[test]
    fn config_seed_overrides_state_for_same_slot() {
        let file: StateFile = toml::from_str(
            r#"
            [[binding]]
            slot = 2
            mac = "AA:BB:CC:DD:EE:01"
            name = "learned-mac"
            [[binding]]
            slot = 3
            mac = "AA:BB:CC:DD:EE:02"
            name = "other"
            "#,
        )
        .unwrap();
        let merged = merge(file, &[target(2, "config-mac", Some(0x99))]);
        assert_eq!(merged[&2].addr, addr(0x99));
        assert!(merged[&2].from_config);
        // Learned entry for another slot survives.
        assert_eq!(merged[&3].addr, addr(0x02));
        assert!(!merged[&3].from_config);
    }

    #[test]
    fn mac_less_config_target_contributes_no_binding() {
        let merged = merge(StateFile::default(), &[target(4, "named-only", None)]);
        assert!(merged.is_empty());
    }

    #[test]
    fn save_skips_config_seeds() {
        let mut bindings = Bindings::new();
        bindings.insert(
            2,
            Binding {
                addr: addr(1),
                name: "seeded".into(),
                from_config: true,
            },
        );
        bindings.insert(
            3,
            Binding {
                addr: addr(2),
                name: "learned".into(),
                from_config: false,
            },
        );
        let file = to_state_file(&bindings);
        assert_eq!(file.bindings.len(), 1);
        assert_eq!(file.bindings[0].slot, 3);
        assert_eq!(file.bindings[0].mac, addr(2).to_string());
    }

    #[test]
    fn state_file_roundtrip() {
        let mut bindings = Bindings::new();
        bindings.insert(
            5,
            Binding {
                addr: addr(7),
                name: "mac".into(),
                from_config: false,
            },
        );
        let text = toml::to_string_pretty(&to_state_file(&bindings)).unwrap();
        let reloaded = merge(toml::from_str(&text).unwrap(), &[]);
        assert_eq!(reloaded, bindings);
    }
}
