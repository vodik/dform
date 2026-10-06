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
    /// the deployment's, `output(Deployment, Key, Value)` (`stack`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, crate::value::Value>,
    /// The outputs declared `secret(T)`: each by its label and the keyed
    /// digest of its value, never the value (E DR-19), and where a provider
    /// holds it when one does (`stack::SecretOutput`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_outputs: BTreeMap<String, crate::stack::SecretOutput>,
    /// The commits the last apply's `git` tables read (`tables::record`),
    /// to say when a ref has moved since; never replayed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub externs: Vec<crate::externs::Answer>,
    /// The values `memo.first` keeps (R-60), by key: a plain one as it is,
    /// a secret one sealed with the stack's key (`memo::Kept`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub memo: BTreeMap<String, crate::memo::Kept>,
    /// The copies whose resources this state holds, by scope (`blue`,
    /// `edge/left`), each with its component's path, as of the last apply:
    /// a plan names a removed copy's deletes as its own (R-67,
    /// `zset::Instances`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instances: BTreeMap<String, String>,
    /// The host keys the built-in `ssh` provider has met, by host as the
    /// program names it (`10.0.0.5`, `db.example.com:2222`): recorded on
    /// first contact, checked on every one after (`plugin::ssh`), reset by
    /// `dform state forget-host HOST`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub known_hosts: BTreeMap<String, KnownHost>,
}

/// A host's key as first met: its type (`ssh-ed25519`), its SHA-256
/// fingerprint as OpenSSH prints it (`SHA256:..`), and when.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownHost {
    pub key_type: String,
    pub fingerprint: String,
    pub when: String,
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
    /// A write-only attribute's digest (R-106, `schema::WRITE_ONLY`), by
    /// path, of the value last applied: the API never answers it, so Plan
    /// compares the program's value with this. Keyed with the stack's key
    /// (`hmac-sha256:..`) when it has one, else `sha256:..`; never the
    /// value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub written: BTreeMap<String, String>,
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

    /// `dform state taint memo KEY`: forget the value `memo.first` keeps
    /// for `key`, so the next run keeps its candidate. Returns what it
    /// kept.
    pub fn taint_memo(&mut self, key: &str) -> Option<crate::memo::Kept> {
        self.memo.remove(key)
    }

    /// `dform state forget-host HOST`: forget the key recorded for `host`,
    /// so the next contact records the one it meets. Returns what it kept.
    pub fn forget_host(&mut self, host: &str) -> Option<KnownHost> {
        self.known_hosts.remove(host)
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
                written: BTreeMap::new(),
            },
        );
    }

    /// Record the digests of `addr`'s write-only attributes as applied
    /// (no-op without an identity).
    pub fn set_written(&mut self, addr: &Address, written: BTreeMap<String, String>) {
        if let Some(e) = self.resources.get_mut(&key(addr)) {
            e.written = written;
        }
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

/// The mock's world file in a deployment's directory: the provider's, not
/// state, so it stays on the machine when the state is in a bucket.
pub const WORLD: &str = "remote.json";

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

/// Where one stack's files live under the state root (`dform.state/`).
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
/// DIR relative to the project root, which holds the state root `root`
/// (`<dir>/dform.state`), not to the working directory.
pub fn local_dir(root: &Path, dir: &Path) -> PathBuf {
    root.parent().unwrap_or(Path::new("")).join(dir)
}

/// A stack whose backend is `local(dir)`: its state and world in `dir`.
pub fn backend_paths(root: &Path, dir: &Path) -> StackPaths {
    StackPaths {
        state: dir.join("state.json"),
        world: dir.join(WORLD),
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

pub fn adopt_map(adopts: &[Adopt]) -> BTreeMap<Address, String> {
    adopts
        .iter()
        .map(|a| (a.addr.clone(), a.remote.clone()))
        .collect()
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}
