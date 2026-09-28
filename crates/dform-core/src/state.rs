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
    /// Objects replaced under `create_before_destroy`: the old object's
    /// identity, kept from the moment its replacement is created until the
    /// deposed object is deleted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deposed: BTreeMap<String, StateEntry>,
    /// The apply in progress, while it runs and after it fails or is killed:
    /// what `apply` needs to resume it (`executor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<InFlight>,
    /// Apply calls whose outcome dform may not know (`Uncertain`), by
    /// address (`key`; a deposed object's delete by `deposed_key`): every
    /// Create and Replace of a tick from before its first call, every other
    /// call from its submission, until the call answers. The next run
    /// resolves them before it plans (`executor::resolve_uncertain`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uncertain: BTreeMap<String, Uncertain>,
    /// How many idempotency keys this state has given out: the next one's
    /// nonce (`new_idempotency_key`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keys: u64,
    /// The stack's outputs as of its last apply: what other stacks read as
    /// `stack_output(Stack, Key, Value)` (`stack`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, crate::value::Value>,
    /// The answers of `persist` externs (E DR-7): kept, and never asked
    /// again, so a generated value stays the same.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub externs: Vec<crate::externs::Answer>,
}

/// The deformations of the current tick that have not been applied yet, each
/// with the world document (the configured attributes Read returned) it was
/// planned against; `None` when the address had no resource in the world.
/// An action leaves it when its Apply call answers. Only these documents are
/// kept, never the stack's, and only until the apply completes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct InFlight {
    pub tick: usize,
    pub remaining: BTreeMap<String, Option<serde_json::Value>>,
}

/// An Apply call that may or may not have taken effect: in flight when dform
/// stopped, or answered with "may have taken effect" (a timeout, a crash).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Uncertain {
    pub op: UncertainOp,
    /// The object the call acts on: an update's or a delete's, a replace's
    /// old one; empty for a create.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remote: String,
    /// A create's or a replace's idempotency key: the retry sends the same
    /// one, so a provider never makes the object twice.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UncertainOp {
    Create,
    Replace { create_first: bool },
    Update,
    Delete,
    DeleteDeposed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEntry {
    pub provider: String,
    pub remote: String,
    /// The addresses this object's document referenced when it was last
    /// applied, so that a delete, which has no desired document left to
    /// read them from, runs before the deletes of what it depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(State {
                version: 1,
                ..State::default()
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

    /// `dform taint`: forget the persisted answer of extern `pred` for the
    /// inputs `args` (each as `--set` would print it: a string bare), so
    /// the next plan asks again. Returns the answer removed.
    pub fn taint(&mut self, pred: &str, args: &[String]) -> Option<crate::externs::Answer> {
        let raw = |v: &crate::value::Value| match v {
            crate::value::Value::Str(s) => s.clone(),
            v => crate::partition::fmt_value(v),
        };
        let i = self.externs.iter().position(|a| {
            a.pred == pred
                && a.inputs.len() == args.len()
                && a.inputs.iter().zip(args).all(|(v, x)| raw(v) == *x)
        })?;
        Some(self.externs.remove(i))
    }

    /// A new idempotency key for `action` of `addr` in the deployment
    /// `stack` (the stack and its key values): a digest of the stack, the
    /// address, the action's own digest (its kind and its changes, a
    /// sensitive one by path only) and a nonce, the count of keys given out
    /// before. The nonce makes it one intended creation's: a later Create
    /// of the same document at the same address (after a delete) is
    /// another creation, and a provider that remembers a client token
    /// longer than its object must not answer it with the first one.
    pub fn new_idempotency_key(
        &mut self,
        stack: &str,
        addr: &Address,
        action: &crate::provider::Action,
    ) -> String {
        let changes: Vec<serde_json::Value> = action
            .changes
            .iter()
            .map(|c| match c.sensitive {
                true => serde_json::json!([c.path]),
                false => serde_json::json!([c.path, c.after]),
            })
            .collect();
        self.keys += 1;
        let v = serde_json::json!({
            "stack": stack,
            "address": key(addr),
            "action": crate::approval::digest_of(&serde_json::json!({
                "kind": format!("{:?}", action.kind),
                "changes": changes,
            })),
            "nonce": self.keys,
        });
        let hex = crate::approval::sha256_hex(crate::approval::canonical_json(&v).as_bytes());
        format!("dform-{}", &hex[..32])
    }

    pub fn get(&self, addr: &Address) -> Option<&StateEntry> {
        self.resources.get(&key(addr))
    }

    pub fn set(&mut self, addr: Address, provider: String, remote: String) {
        let deps = self.get(&addr).map(|e| e.deps.clone()).unwrap_or_default();
        self.resources.insert(
            key(&addr),
            StateEntry {
                provider,
                remote,
                deps,
            },
        );
    }

    /// Record what `addr`'s object depends on (no-op without an identity).
    pub fn set_deps(&mut self, addr: &Address, deps: impl IntoIterator<Item = Address>) {
        if let Some(e) = self.resources.get_mut(&key(addr)) {
            e.deps = deps.into_iter().map(|d| key(&d)).collect();
        }
    }

    /// `moved(T, Old, New)`: New takes Old's identity, so the object is
    /// neither destroyed nor created. Applies only while state maps Old and
    /// not New, so a move already made is a no-op. Returns the moves made.
    pub fn apply_moves(&mut self, moves: &[(Address, Address)]) -> Vec<(Address, Address)> {
        let mut out = Vec::new();
        for (old, new) in moves {
            if self.get(new).is_some() {
                continue;
            }
            if let Some(e) = self.resources.remove(&key(old)) {
                self.resources.insert(key(new), e);
                out.push((old.clone(), new.clone()));
            }
        }
        out
    }

    /// Move `addr`'s identity aside as deposed, making room for its
    /// replacement.
    pub fn depose(&mut self, addr: &Address) {
        if let Some(e) = self.resources.remove(&key(addr)) {
            self.deposed.insert(key(addr), e);
        }
    }

    pub fn deposed_for_provider<'a>(
        &'a self,
        provider: &'a str,
    ) -> impl Iterator<Item = (Address, &'a StateEntry)> + 'a {
        self.deposed
            .iter()
            .filter(move |(_, e)| e.provider == provider)
            .filter_map(|(k, e)| parse_key(k).map(|a| (a, e)))
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

/// A deposed object's key in `State::uncertain`: its address may have a
/// call of its own there too.
pub fn deposed_key(addr: &Address) -> String {
    format!("{}#deposed", key(addr))
}

pub fn parse_key(k: &str) -> Option<Address> {
    let (typ, name) = k.split_once("::")?;
    Some(Address {
        typ: typ.to_string(),
        name: name.to_string(),
    })
}

pub fn state_path(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("state.json")
}

/// The stack a program's state belongs to when it has no `stack` statement:
/// the basename of the program's entry file without its extension,
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
    backend_paths(root, &root.join(stack))
}

/// The directory of a `local(DIR)` backend (a stack's, or a handover's):
/// DIR relative to the directory holding the state root `root`
/// (`<dir>/.dform`: `--root`, else the program's directory), not to the
/// working directory.
pub fn local_dir(root: &Path, dir: &Path) -> PathBuf {
    root.parent().unwrap_or(Path::new("")).join(dir)
}

/// A stack whose backend is `local(dir)`: its state and world in `dir`.
pub fn backend_paths(root: &Path, dir: &Path) -> StackPaths {
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

fn is_zero(n: &u64) -> bool {
    *n == 0
}
