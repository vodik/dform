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
    /// first contact, checked on every one after (`files::ssh`), reset by
    /// `dform state forget-host HOST`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub known_hosts: BTreeMap<String, KnownHost>,
    /// The id of the master the deployment was last applied with
    /// (`custody::id`, public): a run whose master is another refuses
    /// before it plans (R-163).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<String>,
    /// Each secret rotated by `dform secrets rotate` (R-161), by its key
    /// (`random.password("db")`'s `db`, a memo's): its generation, an
    /// input of what `random.*` derives for the key, and who rotated it
    /// when. A key with no record is at generation 1.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, Secret>,
}

/// A secret's rotation (R-161): what `secrets rotate` records, and the
/// audit log's `rotated` entry repeats.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Secret {
    /// 1 for a key never rotated; each rotation adds one.
    pub generation: u32,
    /// The master epoch the key derives from (R-165): pinned by `secrets
    /// cycle` to the epoch it was on; none, the current one (a rotation
    /// moves it there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    /// When it was last rotated: RFC 3339, UTC; empty for a key a cycle
    /// pinned and never rotated.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rotated_at: String,
    /// Who rotated it, as the audit log names an actor (`DFORM_ACTOR`,
    /// else `user@host`): asserted, not verified.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub by: String,
    /// Rotated since the last apply that completed: the next plan carries
    /// the rotation (`rotated/3` to policy).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

impl State {
    /// `dform secrets cycle` (R-165): each of `keys` that derives from the
    /// current epoch pinned to it, `epoch`, before the next becomes
    /// current. The keys pinned.
    pub fn pin(&mut self, keys: impl IntoIterator<Item = String>, epoch: u32) -> Vec<String> {
        let mut out = Vec::new();
        for k in keys {
            let r = self.secrets.entry(k.clone()).or_insert_with(|| Secret {
                generation: 1,
                epoch: None,
                rotated_at: String::new(),
                by: String::new(),
                pending: false,
            });
            if r.epoch.is_none() {
                r.epoch = Some(epoch);
                out.push(k);
            }
        }
        out
    }

    /// The master epochs a secret still derives from (R-165): each pinned
    /// key's, and each kept memo's seal's.
    pub fn epochs_in_use(&self) -> std::collections::BTreeSet<u32> {
        self.secrets
            .values()
            .filter_map(|s| s.epoch)
            .chain(
                self.memo
                    .values()
                    .filter(|m| !m.sealed.is_empty())
                    .map(|m| m.epoch.unwrap_or(1)),
            )
            .collect()
    }
}

impl Secret {
    /// The day it was rotated, as a plan's reason says it.
    pub fn day(&self) -> &str {
        self.rotated_at.get(..10).unwrap_or(&self.rotated_at)
    }
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
    /// Of a destroy (R-149): the next destroy resumes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destroy: bool,
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
    /// compares the program's value with this. Keyed with the
    /// deployment's master (`hmac-sha256:..`); a run that does not hold it
    /// keeps what was there. Never the value, nor an unkeyed digest of it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub written: BTreeMap<String, String>,
    /// Each leaf at a secret path that holds a derived value (`random.*`,
    /// a sealed memo's) as last applied, by path: the digest of the leaf
    /// with each derived value replaced by its stand-in
    /// (`secrets::standin::digest`), a function of public inputs. A run
    /// that does not hold the master proves the leaf unchanged by it
    /// (R-164).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub derived: BTreeMap<String, String>,
    /// Each attribute given at the object's creation only (R-198,
    /// `lifecycle(r, "bootstrap", P)`) that read a `random.*` secret, by
    /// path: the generation of each key it was made with (R-161). An
    /// update keeps it; `secrets list` names the objects made with one
    /// older than the key's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub made_with: BTreeMap<String, BTreeMap<String, u32>>,
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

    /// `dform secrets rotate KEY` (R-161): the key's next generation, on
    /// the current master epoch (R-165), recorded as rotated now by `by`,
    /// and the value `memo.first` keeps for it forgotten, so the next run
    /// keeps its candidate. Returns the record and what the memo kept.
    pub fn rotate(
        &mut self,
        key: &str,
        now: &str,
        by: &str,
    ) -> (Secret, Option<crate::memo::Kept>) {
        let generation = self.secrets.get(key).map_or(1, |s| s.generation) + 1;
        let s = Secret {
            generation,
            epoch: None,
            rotated_at: now.to_string(),
            by: by.to_string(),
            pending: true,
        };
        self.secrets.insert(key.to_string(), s.clone());
        (s, self.memo.remove(key))
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

    pub fn set(&mut self, addr: &Address, provider: String, remote: String) {
        let deps = self.get(addr).map(|e| e.deps.clone()).unwrap_or_default();
        self.resources.insert(
            key(addr),
            StateEntry {
                provider,
                remote,
                deps,
                written: BTreeMap::new(),
                derived: BTreeMap::new(),
                made_with: BTreeMap::new(),
            },
        );
    }

    /// Record the generations of the secrets `addr`'s attributes given at
    /// creation only were made with (no-op without an identity).
    pub fn set_made_with(
        &mut self,
        addr: &Address,
        made_with: BTreeMap<String, BTreeMap<String, u32>>,
    ) {
        if let Some(e) = self.resources.get_mut(&key(addr)) {
            e.made_with = made_with;
        }
    }

    /// Record the digests of `addr`'s write-only attributes as applied, and
    /// the derivation digests of its leaves that hold a derived value
    /// (no-op without an identity).
    pub fn set_written(
        &mut self,
        addr: &Address,
        written: BTreeMap<String, String>,
        derived: BTreeMap<String, String>,
    ) {
        if let Some(e) = self.resources.get_mut(&key(addr)) {
            e.written = written;
            e.derived = derived;
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
    /// Old may be a short name (R-200): the one object of its type whose
    /// name ends in it; two such is an error naming both.
    pub fn apply_moves(&mut self, moves: &[(Address, Address)]) -> Result<Vec<(Address, Address)>> {
        let mut out = Vec::new();
        for (old, new) in moves {
            if self.get(new).is_some() {
                continue;
            }
            let old = self.moved_from(old)?;
            if let Some(e) = self.resources.remove(&key(&old)) {
                self.resources.insert(key(new), e);
                out.push((old, new.clone()));
            }
        }
        Ok(out)
    }

    /// The object a `moved`'s old address names: the address itself when
    /// state maps it, else the one object of its type whose full name ends
    /// in it (`k3s.agent-0.vm` for `vm`); two such is an error naming each.
    fn moved_from(&self, old: &Address) -> Result<Address> {
        if self.get(old).is_some() {
            return Ok(old.clone());
        }
        let suffix = format!(".{}", old.name);
        let ending: Vec<Address> = self
            .resources
            .keys()
            .filter_map(|k| parse_key(k))
            .filter(|a| a.typ == old.typ && a.name.ends_with(&suffix))
            .collect();
        match ending.as_slice() {
            [one] => Ok(one.clone()),
            [] => Ok(old.clone()),
            many => {
                let names: Vec<String> = many.iter().map(crate::report::address).collect();
                anyhow::bail!(
                    "moved({}, {:?}, ..): {} is the short name of {}; name one by its full name",
                    old.typ,
                    old.name,
                    old.name,
                    names.join(" and ")
                )
            }
        }
    }

    /// Move `addr`'s identity aside as deposed, making room for its
    /// replacement.
    pub fn depose(&mut self, addr: &Address) {
        if let Some(e) = self.resources.remove(&key(addr)) {
            self.deposed.insert(key(addr), e);
        }
    }

    pub fn remove(&mut self, addr: &Address) {
        self.resources.remove(&key(addr));
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

/// The directory of a `local(DIR)` backend (a stack's, or a handover's):
/// DIR relative to the project root, which holds the state root `root`
/// (`<dir>/dform.state`), not to the working directory.
pub fn local_dir(root: &Path, dir: &Path) -> PathBuf {
    root.parent().unwrap_or(Path::new("")).join(dir)
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
