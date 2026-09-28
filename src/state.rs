use crate::ir::{Address, Adopt};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub version: u32,
    pub resources: BTreeMap<String, StateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEntry {
    pub provider: String,
    pub remote: String,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(State {
                version: 1,
                resources: BTreeMap::new(),
            });
        }
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let st: State = serde_json::from_slice(&bytes).context("parse state")?;
        Ok(st)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, addr: &Address) -> Option<&StateEntry> {
        self.resources.get(&key(addr))
    }

    pub fn set(&mut self, addr: Address, provider: String, remote: String) {
        self.resources
            .insert(key(&addr), StateEntry { provider, remote });
    }

    pub fn remove(&mut self, addr: &Address) {
        self.resources.remove(&key(addr));
    }

    pub fn entries_for_provider<'a>(
        &'a self,
        provider: &'a str,
    ) -> impl Iterator<Item = (Address, &'a StateEntry)> + 'a {
        self.resources
            .iter()
            .filter(move |(_, e)| e.provider == provider)
            .filter_map(|(k, e)| parse_key(k).map(|a| (a, e)))
    }
}

pub fn key(addr: &Address) -> String {
    format!("{}::{}", addr.typ, addr.name)
}

fn parse_key(k: &str) -> Option<Address> {
    let (typ, name) = k.split_once("::")?;
    Some(Address {
        typ: typ.to_string(),
        name: name.to_string(),
    })
}

pub fn state_path(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("state.json")
}

/// The stack a program's state belongs to. Until a `stack` statement exists
/// this is the basename of the program's entry file without its extension:
/// `dform.df` is `dform`, `pngu.df` is `pngu`.
pub fn stack_name(entry: &Path) -> String {
    entry
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string())
}

/// Where one stack's files live under the state root (`.dform/`).
#[derive(Debug, Clone)]
pub struct StackPaths {
    /// dform's identity mapping for the stack: `<root>/<stack>/state.json`.
    pub state: PathBuf,
    /// The fake provider's world for the stack: `<root>/<stack>/remote.json`.
    pub world: PathBuf,
    /// Discovery facts, shared by every stack: `<root>/inventory.json`.
    pub inventory: PathBuf,
}

pub fn stack_paths(root: &Path, stack: &str) -> StackPaths {
    let dir = root.join(stack);
    StackPaths {
        state: dir.join("state.json"),
        world: dir.join("remote.json"),
        inventory: root.join("inventory.json"),
    }
}

/// A stack whose world is the file `world` (`--world PATH`): its state sits
/// beside it, `<dir>/<stem>.state.json`, so a world file and its identity
/// mapping travel together as one fixture.
pub fn world_paths(root: &Path, world: &Path) -> StackPaths {
    StackPaths {
        state: world.with_extension("state.json"),
        world: world.to_path_buf(),
        inventory: root.join("inventory.json"),
    }
}

/// The stack that inherits state written before state was scoped.
pub const LEGACY_STACK: &str = "dform";

/// Move `<root>/state.json` and `<root>/remote.json`, written before state was
/// scoped to a stack, into the `dform` stack (the default program's). Returns
/// a note for each file moved. A file is left in place if the `dform` stack
/// already has one.
pub fn migrate_unscoped(root: &Path) -> Result<Vec<String>> {
    let target = stack_paths(root, LEGACY_STACK);
    let mut notes = Vec::new();
    for (old, new) in [
        (root.join("state.json"), target.state),
        (root.join("remote.json"), target.world),
    ] {
        if !old.exists() || new.exists() {
            continue;
        }
        if let Some(dir) = new.parent() {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        fs::rename(&old, &new)
            .with_context(|| format!("move {} to {}", old.display(), new.display()))?;
        notes.push(format!(
            "moved unscoped {} to {} (stack '{LEGACY_STACK}')",
            old.display(),
            new.display()
        ));
    }
    Ok(notes)
}

pub fn adopt_map(adopts: &[Adopt]) -> BTreeMap<Address, String> {
    adopts
        .iter()
        .map(|a| (a.addr.clone(), a.remote.clone()))
        .collect()
}
