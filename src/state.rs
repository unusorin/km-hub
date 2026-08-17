//! Learned slot bindings: which Bluetooth device each hotkey slot switches to,
//! and which slot was active last.
//!
//! Bindings come from two sources: `state.toml` (learned via pairing windows,
//! persisted here) and `config.toml` targets with an explicit `mac` (pre-seeds,
//! which override the state file for their slot and are never written back).
//! The active slot is remembered so a restart comes back on the host the user
//! was on — like a multi-host keyboard waking on its last channel — instead of
//! on the local slot with no host connected.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use bluer::Address;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::{LOCAL_SLOT, RemoteTarget};

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
    /// Slot that was active when the state was last saved. Absent in files
    /// written before this was tracked (and while the local slot is active,
    /// see `to_state_file`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_slot: Option<u8>,
    #[serde(rename = "binding", default)]
    bindings: Vec<StateBinding>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateBinding {
    slot: u8,
    mac: String,
    name: String,
}

/// Bindings plus the slot to start on: the last active one if it is still
/// bound, else the local slot.
pub fn load(path: &Path, targets: &[RemoteTarget]) -> Result<(Bindings, u8)> {
    let file: StateFile = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read state file {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("invalid state file {}", path.display()))?
    } else {
        StateFile::default()
    };
    let remembered = file.active_slot;
    let bindings = merge(file, targets);
    let slot = restore_slot(remembered, &bindings);
    Ok((bindings, slot))
}

/// The remembered slot, if it still has a host; otherwise the local slot.
fn restore_slot(remembered: Option<u8>, bindings: &Bindings) -> u8 {
    match remembered {
        Some(slot) if slot == LOCAL_SLOT || bindings.contains_key(&slot) => slot,
        Some(slot) => {
            warn!(slot, "remembered active slot is no longer bound — starting on the local slot");
            LOCAL_SLOT
        }
        None => LOCAL_SLOT,
    }
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
/// stays the source of truth for those) and the active slot. Failure is
/// logged, never fatal.
pub fn save(path: &Path, bindings: &Bindings, active_slot: u8) {
    let text = match toml::to_string_pretty(&to_state_file(bindings, active_slot)) {
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

fn to_state_file(bindings: &Bindings, active_slot: u8) -> StateFile {
    StateFile {
        // The local slot is what a missing key restores to anyway.
        active_slot: (active_slot != LOCAL_SLOT).then_some(active_slot),
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
        let file = to_state_file(&bindings, LOCAL_SLOT);
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
        let text = toml::to_string_pretty(&to_state_file(&bindings, 5)).unwrap();
        let file: StateFile = toml::from_str(&text).unwrap();
        assert_eq!(file.active_slot, Some(5));
        let reloaded = merge(file, &[]);
        assert_eq!(reloaded, bindings);
    }

    #[test]
    fn active_slot_restores_only_while_bound() {
        let mut bindings = Bindings::new();
        bindings.insert(
            3,
            Binding {
                addr: addr(1),
                name: "mac".into(),
                from_config: false,
            },
        );
        assert_eq!(restore_slot(Some(3), &bindings), 3);
        assert_eq!(restore_slot(Some(4), &bindings), LOCAL_SLOT);
        assert_eq!(restore_slot(None, &bindings), LOCAL_SLOT);
        assert_eq!(restore_slot(Some(LOCAL_SLOT), &bindings), LOCAL_SLOT);
    }

    #[test]
    fn a_state_file_without_active_slot_still_loads() {
        let file: StateFile = toml::from_str(
            r#"
            [[binding]]
            slot = 2
            mac = "AA:BB:CC:DD:EE:01"
            name = "tower"
            "#,
        )
        .unwrap();
        assert_eq!(file.active_slot, None);
        // The local slot is not written: a missing key means local.
        assert_eq!(to_state_file(&Bindings::new(), LOCAL_SLOT).active_slot, None);
        assert_eq!(to_state_file(&Bindings::new(), 2).active_slot, Some(2));
    }
}
