//! Who holds a deployment's master (R-163, R-164).
//!
//! A deployment's master is 32 random bytes, [`Key`]: the root of every
//! `random.*` value (HKDF of what it derives for "random master"), the
//! seal of a secret memo, and the key of every digest dform keeps of a
//! secret (a write-only attribute's, a plan file's, the audit log's). It
//! is made once, by the first run that needs one of a deployment that has
//! no state, and never again unless the operator says so: a run whose
//! master is not the one state was applied with refuses before it plans
//! ([`Master::check`]), naming both ids and where this run's came from.
//! `--new-master` accepts the change; then every derived secret changes,
//! and the plan says why.
//!
//! The master id ([`id`]) is public: an HMAC of the `random.*` input key
//! material, so it says nothing of the master, and state records it
//! (`State::master`) with each apply.
//!
//! `RANDOM_MASTER` in the environment is that input key material itself,
//! for tests and the editor. It is taken only where it is what state was
//! applied with (or there is no state): beside a deployment of its own
//! master it is said once, and refused when the ids differ.

use crate::zset::file::Key;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

pub mod given;

/// The master id of the `random.*` input key material `ikm`: an HMAC of
/// it, hex. Public.
pub fn id(ikm: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<sha2::Sha256>>::new_from_slice(ikm).expect("HMAC takes a key of any length");
    mac.update(b"dform master id");
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `n` bytes from the system's random source.
pub fn random_bytes<const N: usize>(what: &str) -> Result<[u8; N]> {
    use std::io::Read;
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .with_context(|| format!("read /dev/urandom for {what}"))?;
    Ok(b)
}

/// What a run asks of the master.
#[derive(Debug, Clone, Copy, Default)]
pub struct Want {
    /// Make one when the deployment has none and no state: a run that
    /// writes (an apply, a plan file), or derives (`random.*`).
    pub make: bool,
    /// `--new-master`: accept a master other than the one state was
    /// applied with, made now when there is none.
    pub new_master: bool,
}

/// The master a run has.
#[derive(Clone, Default)]
pub struct Master {
    /// The key, when the run holds it.
    pub key: Option<Key>,
    /// The `random.*` input key material: `RANDOM_MASTER`, else what the
    /// key derives for it.
    pub random: Option<Vec<u8>>,
    /// The master id of `random`.
    pub id: Option<String>,
    /// Where it came from, as messages say it: `RANDOM_MASTER`, `the key
    /// file s3://../state.key`.
    pub source: String,
    /// Made by this run.
    pub made: bool,
    /// `--new-master` was given.
    pub accept: bool,
    /// Why the run has no key though the deployment has a master (a
    /// passphrase not given): `None` when it has it.
    pub without: Option<String>,
    /// The key is a plain key file the mixing is to seal ([`reseal`]).
    pub unsealed: bool,
    /// The epoch the master is (R-165): 1 until `dform secrets cycle`.
    pub epoch: u32,
    /// The earlier epochs' masters still kept, each by its id: a secret
    /// derived on one keeps it until rotated. The keys when the run holds
    /// the passphrase.
    pub earlier: Vec<Epoch>,
    /// The key every digest of a secret is keyed with (the plan file's,
    /// a write-only attribute's, the audit log's): the first epoch's
    /// master, carried sealed across the epochs after it, so a new epoch
    /// changes no digest. The key itself before a cycle.
    pub digest: Option<Key>,
    /// What the next apply changes of how the master is sealed, when
    /// dform.toml says otherwise than `state.master` ([`reseal`]).
    pub reseal: Option<Reseal>,
}

/// How `state.master`'s seals differ from what the mixing says: a key
/// file to seal, recipients to add or to remove (their keys), the
/// passphrase to add (`Some(true)`) or to drop (`Some(false)`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reseal {
    pub key_file: bool,
    pub added: Vec<Recipient>,
    pub removed: Vec<Recipient>,
    pub passphrase: Option<bool>,
}

impl Reseal {
    fn of(r: Option<&Record>, mixing: &Mixing, key_file: bool) -> Option<Reseal> {
        let had: Vec<Recipient> = r
            .and_then(|r| r.age.as_ref())
            .map(AgeSealed::named)
            .unwrap_or_default();
        let want = mixing.keys();
        let out = Reseal {
            key_file,
            added: mixing
                .recipients
                .iter()
                .filter(|x| !had.iter().any(|h| h.key == x.key))
                .cloned()
                .collect(),
            removed: had.into_iter().filter(|h| !want.contains(&h.key)).collect(),
            passphrase: match (
                r.is_some_and(|r| r.passphrase.is_some()),
                mixing.passphrase.is_some(),
            ) {
                (false, true) => Some(true),
                (true, false) => Some(false),
                _ => None,
            },
        };
        (out != Reseal::default()).then_some(out)
    }

    /// As a run says it, after the deployment's name: `its master is
    /// still a key file in the bucket; the next apply run with
    /// DFORM_PASSPHRASE set seals it and removes the file`, `the next
    /// apply seals it to carol and no longer to bob`. `when` is the apply
    /// (`this apply`, `the next apply`), `place` where the key file is
    /// (`in the bucket`); a passphrase to add names where `mixing` reads it.
    pub fn describe(&self, mixing: &Mixing, when: &str, place: &str) -> String {
        let names = |rs: &[Recipient]| {
            rs.iter()
                .map(Recipient::describe)
                .collect::<Vec<_>>()
                .join(", ")
        };
        // Who seals it under a passphrase: a run that has it.
        let who = match (self.passphrase, &mixing.passphrase) {
            (Some(true), Some(Passphrase::Env(n))) => format!("{when} run with {n} set"),
            (Some(true), Some(Passphrase::Prompt)) => {
                format!("{when}, given the passphrase at its prompt,")
            }
            _ => when.to_string(),
        };
        let mut seal = Vec::new();
        if !self.added.is_empty() {
            seal.push(format!("to {}", names(&self.added)));
        }
        if self.passphrase == Some(true) && !self.key_file {
            seal.push("under the passphrase".to_string());
        }
        let mut does = Vec::new();
        if self.key_file || !seal.is_empty() {
            does.push(match seal.is_empty() {
                true => "seals it".to_string(),
                false => format!("seals it {}", seal.join(" and ")),
            });
        }
        if !self.removed.is_empty() {
            does.push(format!("no longer to {}", names(&self.removed)));
        }
        if self.passphrase == Some(false) {
            does.push("no longer under the passphrase".to_string());
        }
        if self.key_file {
            does.push("removes the file".to_string());
        }
        let does = match does.split_last() {
            Some((last, [])) => last.clone(),
            Some((last, init)) => format!("{} and {last}", init.join(", ")),
            None => String::new(),
        };
        match self.key_file {
            true => format!("its master is still a key file {place}; {who} {does}"),
            false => format!("{who} {does}"),
        }
    }
}

/// An earlier epoch's master (R-165).
#[derive(Clone)]
pub struct Epoch {
    pub epoch: u32,
    pub id: String,
    pub key: Option<Key>,
}

impl Epoch {
    /// The `random.*` input key material of the epoch's master.
    pub fn random(&self) -> Option<Vec<u8>> {
        self.key
            .as_ref()
            .map(|k| crate::secrets::derived(k, "random master").to_vec())
    }
}

impl std::fmt::Debug for Master {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Master")
            .field("id", &self.id)
            .field("source", &self.source)
            .field("made", &self.made)
            .finish_non_exhaustive()
    }
}

impl Master {
    /// The master of a run that has none (an editor, a test of a
    /// program): `random.*` have no value.
    pub fn none() -> Master {
        Master::default()
    }

    /// The master a run whose key is `key` (from `source`) derives with:
    /// `RANDOM_MASTER` when it is set, else the key's.
    fn of(key: Option<Key>, source: String, env: Option<Vec<u8>>) -> Master {
        let (random, source) = match (env, &key) {
            (Some(e), _) => (Some(e), "RANDOM_MASTER".to_string()),
            (None, Some(k)) => (
                Some(crate::secrets::derived(k, "random master").to_vec()),
                source,
            ),
            (None, None) => (None, source),
        };
        Master {
            id: random.as_deref().map(id),
            digest: key.clone(),
            key,
            random,
            source,
            epoch: 1,
            ..Master::default()
        }
    }

    /// Each master the run derives from, by epoch: the earlier ones and
    /// the current; its id and its `random.*` input key material when the
    /// run holds it. What `functions::random::set_epochs` takes.
    pub fn epochs(&self) -> Vec<(u32, String, Option<Vec<u8>>)> {
        let mut out: Vec<(u32, String, Option<Vec<u8>>)> = self
            .earlier
            .iter()
            .map(|e| (e.epoch, e.id.clone(), e.random()))
            .collect();
        if let Some(id) = &self.id {
            out.push((self.epoch.max(1), id.clone(), self.random.clone()));
        }
        out
    }

    /// The key of epoch `epoch`'s master, when the run holds it.
    pub fn key_of(&self, epoch: u32) -> Option<&Key> {
        match epoch == self.epoch.max(1) {
            true => self.key.as_ref(),
            false => self
                .earlier
                .iter()
                .find(|e| e.epoch == epoch)
                .and_then(|e| e.key.as_ref()),
        }
    }

    /// Refuse a master other than the one state was applied with
    /// (`applied`, its id), unless `--new-master` accepts it.
    pub fn check(&self, deployment: &str, applied: Option<&str>) -> Result<()> {
        let (Some(was), Some(now)) = (applied, self.id.as_deref()) else {
            return Ok(());
        };
        // An earlier epoch's (R-165): `secrets cycle` wrote the new one and
        // stopped before state said so.
        if was == now || self.accept || self.earlier.iter().any(|e| e.id == was) {
            return Ok(());
        }
        let fix = match self.source.as_str() {
            "RANDOM_MASTER" => "unset RANDOM_MASTER",
            _ => "restore the key the deployment was applied with",
        };
        bail!(
            "{deployment}: the master this run derives from ({}, id {}) is not the one its state \
             was applied with (id {}): every random.* value and every secret digest would \
             change. {fix}, or run with --new-master to take this master and change them all",
            self.source,
            crate::report::short_id(now),
            crate::report::short_id(was),
        )
    }
}

/// How a deployment's master is kept: dform.toml's `[secrets]` (the
/// stack's `[stacks.NAME.secrets]`, else the project's). With neither a
/// passphrase nor a recipient the master is the key file `state.key`
/// beside the state, on the machine's disk: a local backend's only.
/// Otherwise the backend holds it in `state.master` sealed, never in the
/// clear: under a key scrypt mixes from the passphrase and a salt, and to
/// each age recipient (a team's keys), each of which opens it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mixing {
    /// `[secrets] passphrase`.
    pub passphrase: Option<Passphrase>,
    /// `[secrets] recipients`.
    pub recipients: Vec<Recipient>,
}

/// An age recipient the master is sealed to: an X25519 public key
/// (`age1..`), and the name dform.toml gives it (`recipients = { alice =
/// "age1.." }`), if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub name: Option<String>,
    pub key: String,
}

impl Recipient {
    /// `key` (`age1..`), named `name`.
    pub fn parse(name: Option<&str>, key: &str) -> Result<Recipient> {
        key.parse::<age::x25519::Recipient>()
            .map_err(|e| anyhow::anyhow!("{key:?} is not an age recipient (age1..): {e}"))?;
        Ok(Recipient {
            name: name.map(String::from),
            key: key.to_string(),
        })
    }

    /// As messages name it: its name, else its key shortened.
    pub fn describe(&self) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None => short_key(&self.key),
        }
    }
}

/// An age public key as messages print it: `age1` and its first and last
/// six characters.
pub fn short_key(key: &str) -> String {
    match key.len() > 20 {
        true => format!("{}..{}", &key[..10], &key[key.len() - 6..]),
        false => key.to_string(),
    }
}

/// Where the passphrase comes from: `env:NAME` (a variable, which fnox,
/// `op run` or CI may set), `prompt` (asked on the terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Passphrase {
    Env(String),
    Prompt,
}

impl Passphrase {
    /// `[secrets] passphrase`'s value.
    pub fn parse(s: &str) -> Result<Passphrase> {
        match s.split_once(':') {
            _ if s == "prompt" => Ok(Passphrase::Prompt),
            Some(("env", name))
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                Ok(Passphrase::Env(name.to_string()))
            }
            _ => bail!(
                "{s:?}: where the passphrase comes from, `env:NAME` (a variable) or `prompt` (the \
                 terminal)"
            ),
        }
    }

    /// As messages name it.
    pub fn describe(&self) -> String {
        match self {
            Passphrase::Env(n) => format!("env:{n}"),
            Passphrase::Prompt => "prompt".into(),
        }
    }

    /// The passphrase, or why this run has none.
    fn read(&self, deployment: &str) -> Result<std::result::Result<Vec<u8>, String>> {
        match self {
            Passphrase::Env(n) => Ok(match std::env::var(n) {
                Ok(v) if !v.is_empty() => Ok(v.into_bytes()),
                _ => Err(format!("{n} is not set")),
            }),
            Passphrase::Prompt => prompt(deployment),
        }
    }
}

impl Mixing {
    /// The mixing a project's dform.toml names for the stack `stack`: its
    /// `[stacks.NAME.secrets]`, else the project's `[secrets]`.
    pub fn of(manifest: Option<&crate::project::Manifest>, stack: &str) -> Result<Mixing> {
        let Some(table) = manifest.map(|m| {
            m.stacks
                .get(stack)
                .and_then(|t| t.secrets.as_ref())
                .unwrap_or(&m.secrets)
        }) else {
            return Ok(Mixing::default());
        };
        Ok(Mixing {
            passphrase: table
                .passphrase
                .as_deref()
                .map(Passphrase::parse)
                .transpose()?,
            recipients: table.recipients()?,
        })
    }

    /// The master is the key file: no `[secrets]`.
    pub fn key_file(&self) -> bool {
        self.passphrase.is_none() && self.recipients.is_empty()
    }

    /// The recipients' keys, sorted: what a seal to them records.
    fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.recipients.iter().map(|r| r.key.clone()).collect();
        k.sort();
        k.dedup();
        k
    }

    /// The recipient `key` as dform.toml names it, else shortened.
    pub fn name_of(&self, key: &str) -> String {
        self.recipients
            .iter()
            .find(|r| r.key == key)
            .map_or_else(|| short_key(key), Recipient::describe)
    }

    /// As messages say who opens the master: `the passphrase from
    /// env:NAME`, `age alice, ci`, or both.
    pub fn describe(&self) -> String {
        let mut out = Vec::new();
        if let Some(p) = &self.passphrase {
            out.push(format!("the passphrase from {}", p.describe()));
        }
        if !self.recipients.is_empty() {
            out.push(format!(
                "age {}",
                self.recipients
                    .iter()
                    .map(Recipient::describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.join(" or ")
    }
}

/// The age identities this run holds, and where each came from: the
/// variable `AGE_IDENTITY` (an identity, `AGE-SECRET-KEY-1..`, or the path
/// of a file of them), then each file of the operator's credentials
/// `age/NAME` (`$XDG_CONFIG_HOME/dform/credentials/age/NAME`, R-171), one
/// identity a line, `#` a comment.
pub fn identities() -> Result<Vec<(age::x25519::Identity, String)>> {
    fn parse(text: &str, from: &str, out: &mut Vec<(age::x25519::Identity, String)>) -> Result<()> {
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let id = line.parse::<age::x25519::Identity>().map_err(|e| {
                anyhow::anyhow!(
                    "{from}:{}: not an age identity (AGE-SECRET-KEY-1..): {e}",
                    i + 1
                )
            })?;
            out.push((id, from.to_string()));
        }
        Ok(())
    }
    let mut out = Vec::new();
    if let Some(v) = std::env::var("AGE_IDENTITY").ok().filter(|v| !v.is_empty()) {
        match v.trim_start().starts_with("AGE-SECRET-KEY-") {
            true => parse(&v, "AGE_IDENTITY", &mut out)?,
            false => {
                let text = std::fs::read_to_string(&v)
                    .with_context(|| format!("read AGE_IDENTITY's file {v}"))?;
                parse(&text, &v, &mut out)?
            }
        }
    }
    if let Some(dir) = crate::plugin::credentials::dir().map(|d| d.join("age"))
        && let Ok(files) = std::fs::read_dir(&dir)
    {
        let mut files: Vec<_> = files.filter_map(|f| Some(f.ok()?.path())).collect();
        files.sort();
        for f in files.into_iter().filter(|f| f.is_file()) {
            let text = std::fs::read_to_string(&f)
                .with_context(|| format!("read the age identity {}", f.display()))?;
            parse(
                &text,
                &format!(
                    "age:{}",
                    f.file_name().unwrap_or_default().to_string_lossy()
                ),
                &mut out,
            )?;
        }
    }
    Ok(out)
}

/// What opens a sealed master in this run: the passphrase (read when
/// first needed: `prompt` asks only when no identity opened it) and the
/// age identities.
struct Opener<'a> {
    deployment: &'a str,
    mixing: &'a Mixing,
    pass: std::cell::OnceCell<std::result::Result<Vec<u8>, String>>,
    identities: Vec<(age::x25519::Identity, String)>,
}

impl<'a> Opener<'a> {
    fn new(deployment: &'a str, mixing: &'a Mixing) -> Result<Opener<'a>> {
        Ok(Opener {
            deployment,
            mixing,
            pass: std::cell::OnceCell::new(),
            identities: identities()?,
        })
    }

    /// The passphrase, or why there is none; `None` when the mixing names
    /// none.
    fn pass(&self) -> Result<Option<&std::result::Result<Vec<u8>, String>>> {
        let Some(from) = &self.mixing.passphrase else {
            return Ok(None);
        };
        if self.pass.get().is_none() {
            let _ = self.pass.set(from.read(self.deployment)?);
        }
        Ok(self.pass.get())
    }

    /// The master `passphrase` and `age` seal for `id` (`what` it is, as
    /// messages say it), opened: by an identity, else the passphrase;
    /// `None` when this run has neither. A passphrase that does not open
    /// it is refused.
    fn open(
        &self,
        passphrase: Option<&Sealed>,
        sealed: Option<&AgeSealed>,
        id: &str,
        what: &str,
    ) -> Result<Option<Key>> {
        let checked = |k: Key| -> Result<Option<Key>> {
            match key_id(&k) == id {
                true => Ok(Some(k)),
                false => bail!(
                    "{}: {what} opens to a master whose id is not its own: it was altered",
                    self.deployment
                ),
            }
        };
        if let Some(a) = sealed
            && let Some(k) = open_age(a, &self.identities)?
        {
            return checked(k);
        }
        if let (Some(s), Some(Ok(p))) = (passphrase, self.pass()?) {
            return match open(s, id, p)? {
                Some(k) => checked(k),
                None => bail!(
                    "{}: the passphrase from {} does not open {what} (id {}): not the passphrase \
                     it was sealed with",
                    self.deployment,
                    self.mixing
                        .passphrase
                        .as_ref()
                        .map_or_else(String::new, Passphrase::describe),
                    crate::report::short_id(id)
                ),
            };
        }
        Ok(None)
    }

    /// Why this run cannot open the record `r`.
    fn why(&self, r: Option<&Record>) -> Result<String> {
        let mut why = Vec::new();
        if let Some(Err(w)) = self.pass()? {
            why.push(w.clone());
        }
        if r.is_some_and(|r| r.passphrase.is_none()) && self.mixing.passphrase.is_some() {
            why.push("it is not sealed under the passphrase yet".into());
        }
        if !self.mixing.recipients.is_empty() || r.is_some_and(|r| r.age.is_some()) {
            why.push(match self.identities.is_empty() {
                true => "no age identity (AGE_IDENTITY, or a credential age:NAME)".into(),
                false => format!(
                    "no age identity it is sealed to ({} tried)",
                    self.identities
                        .iter()
                        .map(|(_, f)| f.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Ok(why.join("; "))
    }

    /// Whether this run has a means to open a master.
    fn able(&self) -> Result<bool> {
        Ok(!self.identities.is_empty() || matches!(self.pass()?, Some(Ok(_))))
    }
}

/// A master sealed to age recipients: their keys, sorted, and the age
/// file (base64) whose payload is the master.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgeSealed {
    pub recipients: Vec<String>,
    /// The name dform.toml gives each recipient, by key: what a message
    /// names one removed since by.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub names: std::collections::BTreeMap<String, String>,
    pub sealed: String,
}

impl AgeSealed {
    /// The recipients, each with its name.
    fn named(&self) -> Vec<Recipient> {
        self.recipients
            .iter()
            .map(|k| Recipient {
                name: self.names.get(k).cloned(),
                key: k.clone(),
            })
            .collect()
    }
}

/// `key` sealed to `recipients` (their `age1..` keys, sorted).
fn seal_age(
    key: &Key,
    recipients: &[String],
    names: BTreeMap<String, String>,
) -> Result<AgeSealed> {
    use base64::Engine;
    use std::io::Write;
    let parsed: Vec<age::x25519::Recipient> = recipients
        .iter()
        .map(|r| {
            r.parse()
                .map_err(|e| anyhow::anyhow!("{r:?} is not an age recipient: {e}"))
        })
        .collect::<Result<_>>()?;
    let enc = age::Encryptor::with_recipients(parsed.iter().map(|r| r as &dyn age::Recipient))
        .map_err(|e| anyhow::anyhow!("seal the master to its recipients: {e}"))?;
    let mut out = Vec::new();
    let mut w = enc.wrap_output(&mut out)?;
    w.write_all(&key.bytes())?;
    w.finish()?;
    Ok(AgeSealed {
        recipients: recipients.to_vec(),
        names,
        sealed: base64::engine::general_purpose::STANDARD.encode(out),
    })
}

/// The master `s` seals, opened with one of `identities`; `None` when it
/// is sealed to none of them.
fn open_age(s: &AgeSealed, identities: &[(age::x25519::Identity, String)]) -> Result<Option<Key>> {
    use base64::Engine;
    use std::io::Read;
    if identities.is_empty() {
        return Ok(None);
    }
    let b = base64::engine::general_purpose::STANDARD
        .decode(&s.sealed)
        .map_err(|e| anyhow::anyhow!("the master sealed to age recipients is not base64: {e}"))?;
    let d = age::Decryptor::new(&b[..])
        .map_err(|e| anyhow::anyhow!("the master sealed to age recipients: {e}"))?;
    let mut r = match d.decrypt(identities.iter().map(|(i, _)| i as &dyn age::Identity)) {
        Ok(r) => r,
        Err(age::DecryptError::NoMatchingKeys) => return Ok(None),
        Err(e) => bail!("the master sealed to age recipients: {e}"),
    };
    let mut plain = Vec::new();
    r.read_to_end(&mut plain)?;
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("the master sealed to age recipients is not 32 bytes"))?;
    Ok(Some(Key::from(bytes)))
}

/// The seals of a master `key` (id `id`) as `mixing` says, `kept` the
/// ones it has (the same master's): one kept where it is still what the
/// mixing says, a new one otherwise; the passphrase's only when this run
/// has it (else the next run that has it adds it).
fn seals(
    key: &Key,
    id: &str,
    opener: &Opener,
    kept: (Option<&Sealed>, Option<&AgeSealed>),
) -> Result<(Option<Sealed>, Option<AgeSealed>)> {
    let passphrase = match (&opener.mixing.passphrase, kept.0) {
        (None, _) => None,
        (Some(_), Some(s)) => Some(s.clone()),
        (Some(_), None) => match opener.pass()? {
            Some(Ok(p)) => Some(seal(key, id, p)?),
            _ => None,
        },
    };
    let keys = opener.mixing.keys();
    let names: BTreeMap<String, String> = opener
        .mixing
        .recipients
        .iter()
        .filter_map(|r| Some((r.key.clone(), r.name.clone()?)))
        .collect();
    let sealed = match kept.1 {
        _ if keys.is_empty() => None,
        Some(a) if a.recipients == keys => Some(AgeSealed { names, ..a.clone() }),
        _ => Some(seal_age(key, &keys, names)?),
    };
    Ok((passphrase, sealed))
}

/// The passphrase asked on the terminal, once a process: with no terminal,
/// none.
fn prompt(deployment: &str) -> Result<std::result::Result<Vec<u8>, String>> {
    use std::io::{BufRead, IsTerminal, Write};
    static ANSWER: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);
    if let Some(a) = ANSWER.lock().expect("prompt").clone() {
        return Ok(Ok(a));
    }
    if !std::io::stdin().is_terminal() {
        return Ok(Err("there is no terminal to ask the passphrase on".into()));
    }
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("open the terminal to ask the passphrase")?;
    let mut out = tty.try_clone()?;
    write!(out, "passphrase of {deployment}'s master: ")?;
    out.flush()?;
    let echo = Echo::off(&tty);
    let mut line = String::new();
    std::io::BufReader::new(&tty).read_line(&mut line)?;
    drop(echo);
    writeln!(out)?;
    let a = line.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
    if a.is_empty() {
        return Ok(Err("no passphrase was given".into()));
    }
    *ANSWER.lock().expect("prompt") = Some(a.clone());
    Ok(Ok(a))
}

/// The terminal's echo, off until dropped.
struct Echo {
    fd: i32,
    was: Option<libc::termios>,
}

impl Echo {
    fn off(tty: &std::fs::File) -> Echo {
        use std::os::fd::AsRawFd;
        let fd = tty.as_raw_fd();
        // SAFETY: `t` is written by tcgetattr before it is read; `fd` is
        // the open terminal's.
        let was = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            (libc::tcgetattr(fd, &mut t) == 0).then_some(t)
        };
        if let Some(mut t) = was {
            t.c_lflag &= !libc::ECHO;
            // SAFETY: as above.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
        }
        Echo { fd, was }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        if let Some(t) = self.was {
            // SAFETY: restores the attributes read in `off`.
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &t) };
        }
    }
}

/// `state.master`: what the backend keeps of a deployment's master. Its
/// id, public; the master sealed under the passphrase's mixing, never in
/// the clear. A deployment whose master is a key file has none.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub version: u32,
    /// The master id ([`id`]), as state records it.
    pub id: String,
    /// The public key another stack seals an output to this deployment
    /// with ([`seal_to`], R-166): X25519, which the master derives; hex.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub public: String,
    /// The master sealed under a key scrypt mixes from the passphrase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<Sealed>,
    /// The master sealed to the age recipients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age: Option<AgeSealed>,
    /// Which epoch this master is (R-165): 1 until `dform secrets cycle`.
    #[serde(default = "first_epoch", skip_serializing_if = "is_first_epoch")]
    pub epoch: u32,
    /// The earlier epochs' masters a secret still derives from, each
    /// sealed as the current one is; one no secret derives from is retired
    /// (deleted) by the apply that moves its last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<EarlierRecord>,
    /// The digest key (the first epoch's master), sealed under this
    /// master (`secrets::seal`, [`DIGEST_ROOT`]): after a cycle.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
}

/// An earlier epoch in `state.master` (R-165).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EarlierRecord {
    pub epoch: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<Sealed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age: Option<AgeSealed>,
}

impl Record {
    /// The backend holds the master sealed, not as a key file.
    pub fn sealed(&self) -> bool {
        self.passphrase.is_some() || self.age.is_some()
    }
}

fn first_epoch() -> u32 {
    1
}

fn is_first_epoch(e: &u32) -> bool {
    *e == 1
}

/// What the digest key is sealed as under a later epoch's master.
const DIGEST_ROOT: &str = "dform digest root";

/// A master sealed under a passphrase: XChaCha20-Poly1305 under
/// scrypt(passphrase, salt), the master id the associated data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sealed {
    /// `scrypt`, its parameters.
    pub kdf: String,
    pub log_n: u8,
    pub r: u32,
    pub p: u32,
    /// 16 random bytes, hex.
    pub salt: String,
    /// The nonce and the ciphertext with its tag, base64.
    pub sealed: String,
}

/// scrypt's cost: OWASP's (2^17, 8, 1), 128 MiB.
const LOG_N: u8 = 17;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The key scrypt mixes from `pass` with `s`'s salt and cost.
fn mixed(pass: &[u8], s: &Sealed) -> Result<[u8; 32]> {
    if s.kdf != "scrypt" {
        bail!("the master is sealed with {:?}, not scrypt", s.kdf);
    }
    let salt = unhex(&s.salt).ok_or_else(|| anyhow::anyhow!("the salt is not hex"))?;
    let params = scrypt::Params::new(s.log_n, s.r, s.p)
        .map_err(|e| anyhow::anyhow!("scrypt's parameters: {e}"))?;
    let mut out = [0u8; 32];
    scrypt::scrypt(pass, &salt, &params, &mut out).map_err(|e| anyhow::anyhow!("scrypt: {e}"))?;
    Ok(out)
}

fn cipher(k: &[u8; 32]) -> chacha20poly1305::XChaCha20Poly1305 {
    use chacha20poly1305::KeyInit;
    chacha20poly1305::XChaCha20Poly1305::new_from_slice(k).expect("a 32-byte key")
}

/// `key`, whose master id is `id`, sealed under `pass`.
fn seal(key: &Key, id: &str, pass: &[u8]) -> Result<Sealed> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    let mut s = Sealed {
        kdf: "scrypt".into(),
        log_n: LOG_N,
        r: 8,
        p: 1,
        salt: hex(&random_bytes::<16>("the passphrase's salt")?),
        sealed: String::new(),
    };
    let nonce = random_bytes::<24>("the master's seal")?;
    let ct = cipher(&mixed(pass, &s)?)
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &key.bytes(),
                aad: id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("seal the master"))?;
    let mut b = nonce.to_vec();
    b.extend_from_slice(&ct);
    s.sealed = base64::engine::general_purpose::STANDARD.encode(b);
    Ok(s)
}

/// The master `s` seals for `id`, opened with `pass`; `None` when the
/// passphrase is not the one it was sealed with.
fn open(s: &Sealed, id: &str, pass: &[u8]) -> Result<Option<Key>> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    let b = base64::engine::general_purpose::STANDARD
        .decode(&s.sealed)
        .map_err(|e| anyhow::anyhow!("the sealed master is not base64: {e}"))?;
    if b.len() < 24 {
        bail!("the sealed master is too short");
    }
    let (nonce, ct) = b.split_at(24);
    let Ok(plain) = cipher(&mixed(pass, s)?).decrypt(
        nonce.into(),
        Payload {
            msg: ct,
            aad: id.as_bytes(),
        },
    ) else {
        return Ok(None);
    };
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("the sealed master is not 32 bytes"))?;
    Ok(Some(Key::from(bytes)))
}

/// The master `r` keeps sealed, opened with `pass`: `None` when it is not
/// the passphrase. What a test or a tool of the operator's opens a
/// bucket's record with.
pub fn unseal(r: &Record, pass: &[u8]) -> Result<Option<Key>> {
    match &r.passphrase {
        Some(s) => open(s, &r.id, pass),
        None => Ok(None),
    }
}

/// The master id of a key: of what it derives for `random.*`.
fn key_id(k: &Key) -> String {
    id(&crate::secrets::derived(k, "random master"))
}

/// `state.master`, with its version.
fn load_record(store: &dyn crate::store::Store) -> Result<Option<(Record, String)>> {
    use crate::store::MASTER;
    let Some(o) = store.get(MASTER)? else {
        return Ok(None);
    };
    let r = serde_json::from_slice(&o.bytes)
        .with_context(|| format!("parse {}", store.locate(MASTER)))?;
    Ok(Some((r, o.etag)))
}

fn record_bytes(r: &Record) -> Vec<u8> {
    let mut b = serde_json::to_vec_pretty(r).expect("a record serializes");
    b.push(b'\n');
    b
}

/// The master of the deployment whose objects are `store`'s, kept as
/// `mixing` says: `applied` says whether it was ever applied with one. A
/// master is read (a sealed one opened with an age identity or the
/// passphrase); one is made only for a deployment never applied
/// (`want.make`), or when `--new-master` asks for it. A run that can open
/// none has no key: `Master::without` says why. One whose seals are not
/// what the mixing says (a recipient added or removed, a key file to
/// seal) says so in `Master::reseal`, which the next apply writes.
pub fn resolve(
    store: &dyn crate::store::Store,
    deployment: &str,
    applied: &dyn Fn() -> Result<bool>,
    mixing: &Mixing,
    want: Want,
) -> Result<Master> {
    let env = std::env::var("RANDOM_MASTER")
        .ok()
        .filter(|m| !m.is_empty())
        .map(String::into_bytes);
    let mut r = Resolving {
        store,
        deployment,
        applied,
        mixing,
        want,
        record: load_record(store)?,
        out: Master {
            accept: want.new_master,
            ..Master::default()
        },
    };
    let plain = Key::load(store)?;
    let key = match mixing.key_file() {
        true => r.key_file(plain)?,
        false => r.sealed(plain)?,
    };
    if env.is_some() && (key.is_some() || r.out.id.is_some()) {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "warning: RANDOM_MASTER is set: random.* derive from it, not from {deployment}'s \
                 own master"
            )
        });
    }
    if let (true, Some(k)) = (want.make || want.new_master, &key) {
        r.publish(k)?;
    }
    r.into_master(key, env)
}

/// A master being resolved: where it is kept, what the run wants of it,
/// its record as read, and the master as far as it is known.
struct Resolving<'a> {
    store: &'a dyn crate::store::Store,
    deployment: &'a str,
    /// Whether the deployment was ever applied.
    applied: &'a dyn Fn() -> Result<bool>,
    mixing: &'a Mixing,
    want: Want,
    record: Option<(Record, String)>,
    out: Master,
}

impl Resolving<'_> {
    /// The master is missing, and the deployment was applied with it.
    fn missing(&self) -> anyhow::Error {
        use crate::store::{KEY, MASTER};
        anyhow::anyhow!(
            "{}: {} is missing, and the deployment was applied with it: every random.* value \
             and every secret digest derives from it. Restore it from a backup of the state \
             (state and key go together), or run with --new-master to make a new one and \
             change every derived secret on purpose",
            self.deployment,
            match self.mixing.key_file() {
                true => self.store.locate(KEY),
                false => self.store.locate(MASTER),
            }
        )
    }

    /// A master the mixing keeps as a plain key file: read, or made for a
    /// deployment never applied; one the bucket holds is said to be read
    /// access to every derived secret.
    fn key_file(&mut self, plain: Option<Key>) -> Result<Option<Key>> {
        use crate::store::{Cond, KEY, MASTER};
        let (store, deployment, want) = (self.store, self.deployment, self.want);
        if let Some((r, _)) = self.record.as_ref().filter(|(r, _)| r.sealed()) {
            bail!(
                "{deployment}: {} holds its master sealed (id {}), and dform.toml names neither \
                 a passphrase nor recipients: add `[secrets] passphrase = \"env:NAME\"` (or \
                 \"prompt\"), or `recipients = [\"age1..\"]`",
                store.locate(MASTER),
                crate::report::short_id(&r.id)
            );
        }
        self.out.source = format!("the key file {}", store.locate(KEY));
        Ok(match plain {
            Some(k) => {
                if !store.local() {
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| {
                        eprintln!(
                            "warning: {deployment}'s master is the key file {}, in the bucket \
                             beside the state: read access to the state is read access to \
                             every derived secret. Set `[secrets] passphrase` (or `recipients`) \
                             in dform.toml; the next apply seals it",
                            store.locate(KEY)
                        )
                    });
                }
                Some(k)
            }
            None if (self.applied)()? && !want.new_master => return Err(self.missing()),
            None if want.make || want.new_master => {
                if !store.local() {
                    bail!(
                        "{deployment}: a new master would be the key file {}, in the bucket \
                         beside the state, where read access to the state is read access to \
                         every derived secret: set `[secrets] passphrase = \"env:NAME\"` (or \
                         \"prompt\", or `recipients = [\"age1..\"]`) in dform.toml, and the \
                         bucket keeps it sealed",
                        store.locate(KEY)
                    );
                }
                let key = fresh_key()?;
                match store
                    .put(KEY, &key.bytes(), &Cond::IfAbsent)
                    .with_context(|| format!("write the key {}", store.locate(KEY)))?
                {
                    Some(_) => {
                        self.out.made = true;
                        Some(key)
                    }
                    // Made by another run meanwhile: that one is the key.
                    None => {
                        Some(Key::load(store)?.ok_or_else(|| {
                            anyhow::anyhow!("the key {}: gone", store.locate(KEY))
                        })?)
                    }
                }
            }
            None => None,
        })
    }

    /// A master the mixing keeps sealed (to a passphrase, to recipients):
    /// opened with each earlier epoch's (R-165); a key file a sealing left
    /// behind goes with the next apply; one made for a deployment never
    /// applied.
    fn sealed(&mut self, plain: Option<Key>) -> Result<Option<Key>> {
        use crate::store::{KEY, MASTER};
        let (store, deployment, mixing, want) =
            (self.store, self.deployment, self.mixing, self.want);
        let opener = Opener::new(deployment, mixing)?;
        self.out.source = format!("{} sealed for {}", store.locate(MASTER), mixing.describe());
        Ok(match (&self.record, plain) {
            (Some((r, _)), plain) if r.sealed() => {
                self.out.id = Some(r.id.clone());
                self.out.epoch = r.epoch;
                // The earlier epochs (R-165): their ids, and their keys
                // when the run opens them.
                for e in &r.earlier {
                    let what = format!("{}'s epoch {}", store.locate(MASTER), e.epoch);
                    let key = opener.open(e.passphrase.as_ref(), e.age.as_ref(), &e.id, &what)?;
                    self.out.earlier.push(Epoch {
                        epoch: e.epoch,
                        id: e.id.clone(),
                        key,
                    });
                }
                // A key file a sealing left behind goes with the next
                // apply ([`reseal`]).
                self.out.unsealed = plain.is_some();
                let key = opener.open(
                    r.passphrase.as_ref(),
                    r.age.as_ref(),
                    &r.id,
                    &store.locate(MASTER),
                )?;
                if key.is_none() {
                    self.out.without = Some(opener.why(Some(r))?);
                }
                self.out.reseal = Reseal::of(Some(r), mixing, self.out.unsealed);
                key
            }
            // Sealed by the next apply that can.
            (_, Some(k)) => {
                self.out.source = format!("the key file {}", store.locate(KEY));
                self.out.unsealed = true;
                self.out.reseal = Reseal::of(None, mixing, true);
                Some(k)
            }
            (_, None) if (self.applied)()? && !want.new_master => return Err(self.missing()),
            (_, None) if want.make || want.new_master => self.make_sealed(&opener)?,
            (_, None) => {
                if !opener.able()? {
                    self.out.without = Some(opener.why(None)?);
                }
                None
            }
        })
    }

    /// A new master, sealed as the mixing says: to the recipients needs
    /// only their public keys, under the passphrase needs it. None, and
    /// why, when it cannot be sealed.
    fn make_sealed(&mut self, opener: &Opener) -> Result<Option<Key>> {
        use crate::store::{Cond, MASTER};
        let (store, deployment, mixing) = (self.store, self.deployment, self.mixing);
        let key = fresh_key()?;
        let id = key_id(&key);
        let (passphrase, age) = seals(&key, &id, opener, (None, None))?;
        if passphrase.is_none() && age.is_none() {
            self.out.without = Some(opener.why(None)?);
            return Ok(None);
        }
        let r = Record {
            version: 1,
            passphrase,
            age,
            public: hex(&seal_pair(&key).1),
            id,
            epoch: 1,
            earlier: Vec::new(),
            digest: String::new(),
        };
        let cond = match &self.record {
            Some((_, etag)) => Cond::IfMatch(etag.clone()),
            None => Cond::IfAbsent,
        };
        match store
            .put(MASTER, &record_bytes(&r), &cond)
            .with_context(|| format!("write {}", store.locate(MASTER)))?
        {
            Some(_) => {
                self.out.made = true;
                self.out.reseal = Reseal::of(Some(&r), mixing, false);
                Ok(Some(key))
            }
            None => bail!(
                "{deployment}: {} was written by another run meanwhile: run again",
                store.locate(MASTER)
            ),
        }
    }

    /// The public key other stacks seal to (R-166), kept beside the master
    /// by a run that writes: a key file's deployment gets a record of its
    /// own, its id and public key only. Another run's write meanwhile is as
    /// good.
    fn publish(&self, k: &Key) -> Result<()> {
        use crate::store::{Cond, MASTER};
        let store = self.store;
        let public = hex(&seal_pair(k).1);
        let fresh = load_record(store)?;
        if fresh.as_ref().is_none_or(|(r, _)| r.public != public) {
            let (r, cond) = match fresh {
                Some((r, etag)) => (
                    Record {
                        public: public.clone(),
                        ..r
                    },
                    Cond::IfMatch(etag),
                ),
                None => (
                    Record {
                        version: 1,
                        id: key_id(k),
                        public: public.clone(),
                        passphrase: None,
                        age: None,
                        epoch: 1,
                        earlier: Vec::new(),
                        digest: String::new(),
                    },
                    Cond::IfAbsent,
                ),
            };
            // Another run's write meanwhile is as good.
            store
                .put(MASTER, &record_bytes(&r), &cond)
                .with_context(|| format!("write {}", store.locate(MASTER)))?;
        }
        Ok(())
    }

    /// The master resolved: its digest key, after a cycle the first
    /// epoch's, sealed under the current master.
    fn into_master(self, key: Option<Key>, env: Option<Vec<u8>>) -> Result<Master> {
        let of = Master::of(key, self.out.source.clone(), env);
        // After a cycle the digest key is the first epoch's, sealed under the
        // current master.
        let digest = match (&self.record, &of.key) {
            (Some((r, _)), Some(k)) if !r.digest.is_empty() => {
                let b: [u8; 32] = crate::secrets::open(k, DIGEST_ROOT, &r.digest)?
                    .try_into()
                    .map_err(|_| {
                        anyhow::anyhow!("{}: the digest key is not 32 bytes", self.deployment)
                    })?;
                Some(Key::from(b))
            }
            _ => of.digest,
        };
        let out = self.out;
        Ok(Master {
            id: of.id.or(out.id),
            key: of.key,
            random: of.random,
            source: of.source,
            digest,
            epoch: out.epoch.max(1),
            ..out
        })
    }
}

/// A new key, 32 random bytes.
fn fresh_key() -> Result<Key> {
    Ok(Key::from(random_bytes::<32>("the master")?))
}

/// `dform secrets cycle` (R-165): a new master, epoch N+1, sealed as the
/// mixing says beside epoch N, which stays (sealed) while a secret derives
/// from it; the digest key carried over. Its epoch and id.
pub fn cycle(
    store: &dyn crate::store::Store,
    deployment: &str,
    master: &Master,
    mixing: &Mixing,
) -> Result<(u32, String)> {
    use crate::store::{Cond, MASTER};
    if mixing.key_file() {
        bail!(
            "{deployment}: its master is the key file {}: an epoch is kept sealed beside the \
             next, so cycling needs `[secrets] passphrase` or `recipients` in dform.toml (the \
             next apply seals the key file)",
            store.locate(crate::store::KEY)
        );
    }
    let (Some(_), Some(digest)) = (&master.key, &master.digest) else {
        bail!(
            "{deployment}: cycling seals a new master and needs the current one ({})",
            master.without.as_deref().unwrap_or("not held")
        );
    };
    let Some((r, etag)) = load_record(store)? else {
        bail!("{deployment}: {} is missing", store.locate(MASTER));
    };
    if !r.sealed() || master.unsealed {
        bail!(
            "{deployment}: its master is not sealed yet: apply once with the passphrase (or an \
             age identity), then cycle"
        );
    }
    let opener = Opener::new(deployment, mixing)?;
    if let Some(Err(why)) = opener.pass()? {
        bail!("{deployment}: cycling needs the passphrase: {why}");
    }
    let new = Key::from(random_bytes::<32>("the master")?);
    let id = key_id(&new);
    let (passphrase, age) = seals(&new, &id, &opener, (None, None))?;
    let mut earlier = r.earlier.clone();
    earlier.push(EarlierRecord {
        epoch: r.epoch,
        id: r.id.clone(),
        passphrase: r.passphrase.clone(),
        age: r.age.clone(),
    });
    let next = Record {
        version: 1,
        passphrase,
        age,
        public: hex(&seal_pair(&new).1),
        epoch: r.epoch + 1,
        earlier,
        digest: crate::secrets::seal(&new, DIGEST_ROOT, &digest.bytes())?,
        id: id.clone(),
    };
    if store
        .put(MASTER, &record_bytes(&next), &Cond::IfMatch(etag))
        .with_context(|| format!("write {}", store.locate(MASTER)))?
        .is_none()
    {
        bail!(
            "{deployment}: {} was written by another run meanwhile: run again",
            store.locate(MASTER)
        );
    }
    Ok((next.epoch, id))
}

/// Delete each earlier epoch no secret derives from (`keep` the epochs
/// one does): what the apply that moves an epoch's last secret does
/// (R-165). The epochs retired, with their ids.
pub fn retire(
    store: &dyn crate::store::Store,
    keep: &std::collections::BTreeSet<u32>,
) -> Result<Vec<(u32, String)>> {
    use crate::store::{Cond, MASTER};
    let Some((mut r, etag)) = load_record(store)? else {
        return Ok(Vec::new());
    };
    let (kept, gone): (Vec<EarlierRecord>, Vec<EarlierRecord>) =
        r.earlier.drain(..).partition(|e| keep.contains(&e.epoch));
    if gone.is_empty() {
        return Ok(Vec::new());
    }
    r.earlier = kept;
    if store
        .put(MASTER, &record_bytes(&r), &Cond::IfMatch(etag))
        .with_context(|| format!("write {}", store.locate(MASTER)))?
        .is_none()
    {
        // Another run's write meanwhile: the next apply retires it.
        return Ok(Vec::new());
    }
    Ok(gone.into_iter().map(|e| (e.epoch, e.id)).collect())
}

/// Seal the master as `mixing` says, what the apply after a change of
/// dform.toml's `[secrets]` does: a plain key file sealed (the same
/// master, so nothing derived changes) and removed (R-164), the master
/// and each earlier epoch sealed again to the recipients now named (After
/// R-164). A recipient removed is sealed to no longer, which revokes
/// nobody who opened it before: `secrets cycle` makes a master they never
/// held. What it did, when it did anything; nothing when this run does
/// not hold every epoch's master.
pub fn reseal(
    store: &dyn crate::store::Store,
    deployment: &str,
    master: &Master,
    mixing: &Mixing,
) -> Result<Option<Reseal>> {
    use crate::store::{Cond, KEY, MASTER};
    let (true, Some(key), false) = (master.reseal.is_some(), &master.key, mixing.key_file()) else {
        return Ok(None);
    };
    if master.earlier.iter().any(|e| e.key.is_none()) {
        return Ok(None);
    }
    let opener = Opener::new(deployment, mixing)?;
    let id = key_id(key);
    let record = load_record(store)?;
    let current = record.as_ref().filter(|(r, _)| r.sealed() && r.id == id);
    let (passphrase, age) = seals(
        key,
        &id,
        &opener,
        current.map_or((None, None), |(r, _)| {
            (r.passphrase.as_ref(), r.age.as_ref())
        }),
    )?;
    if passphrase.is_none() && age.is_none() {
        return Ok(None);
    }
    let mut earlier = Vec::new();
    for e in master.earlier.iter() {
        let was = current.and_then(|(r, _)| r.earlier.iter().find(|x| x.epoch == e.epoch));
        let k = e.key.as_ref().expect("checked above");
        let (p, a) = seals(
            k,
            &e.id,
            &opener,
            was.map_or((None, None), |w| (w.passphrase.as_ref(), w.age.as_ref())),
        )?;
        earlier.push(EarlierRecord {
            epoch: e.epoch,
            id: e.id.clone(),
            passphrase: p,
            age: a,
        });
    }
    let r = Record {
        version: 1,
        passphrase,
        age,
        public: hex(&seal_pair(key).1),
        id: id.clone(),
        epoch: current.map_or(1, |(r, _)| r.epoch),
        earlier,
        digest: current.map(|(r, _)| r.digest.clone()).unwrap_or_default(),
    };
    let done = Reseal::of(current.map(|(r, _)| r), mixing, master.unsealed).map(|d| Reseal {
        // What could not be done yet (the passphrase without it) stays.
        passphrase: match (d.passphrase, &r.passphrase) {
            (Some(true), None) => None,
            (p, _) => p,
        },
        ..d
    });
    if current.is_some_and(|(c, _)| *c == r) && !master.unsealed {
        return Ok(None);
    }
    let cond = match record {
        Some((_, etag)) => Cond::IfMatch(etag),
        None => Cond::IfAbsent,
    };
    if store
        .put(MASTER, &record_bytes(&r), &cond)
        .with_context(|| format!("write {}", store.locate(MASTER)))?
        .is_none()
    {
        bail!(
            "{} was written by another run while the master was being sealed: run again",
            store.locate(MASTER)
        );
    }
    if master.unsealed {
        store.delete(KEY)?;
    }
    Ok(Some(done.unwrap_or_default()))
}

/// Who opens one epoch of a deployment's master, and who could: what
/// `secrets list` prints, the offboarding list (R-165, After R-164).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holders {
    pub epoch: u32,
    pub id: String,
    pub current: bool,
    /// What opens it now: `the passphrase (env:NAME)`, each recipient.
    pub opens: Vec<String>,
    /// Each recipient removed since the epoch began, with when: it could
    /// open it, and may have kept it.
    pub could: Vec<(String, String)>,
}

/// The holders of each epoch `state.master` keeps, current first; `log`
/// the deployment's audit log (`recipients` entries say who was removed
/// when, `cycled` ones when each epoch began). Empty for a key file.
pub fn holders(
    store: &dyn crate::store::Store,
    mixing: &Mixing,
    log: &[serde_json::Value],
) -> Result<Vec<Holders>> {
    let Some((r, _)) = load_record(store)? else {
        return Ok(Vec::new());
    };
    if !r.sealed() {
        return Ok(Vec::new());
    }
    // A key's name: dform.toml's, the record's, else the one the log gave
    // it when added.
    let known: BTreeMap<&String, &String> = std::iter::once(&r.age)
        .chain(r.earlier.iter().map(|e| &e.age))
        .flatten()
        .flat_map(|a| a.names.iter())
        .collect();
    let name = |key: &str| -> String {
        if mixing.recipients.iter().any(|x| x.key == key) {
            return mixing.name_of(key);
        }
        if let Some(n) = known.get(&key.to_string()) {
            return n.to_string();
        }
        log.iter()
            .filter(|e| e["kind"] == "recipients")
            .flat_map(|e| e["added"].as_array().cloned().unwrap_or_default())
            .find(|a| a["key"] == key)
            .and_then(|a| a["name"].as_str().map(String::from))
            .unwrap_or_else(|| short_key(key))
    };
    let opens = |p: Option<&Sealed>, a: Option<&AgeSealed>| -> Vec<String> {
        let mut out = Vec::new();
        if p.is_some() {
            out.push(match &mixing.passphrase {
                Some(from) => format!("the passphrase ({})", from.describe()),
                None => "the passphrase".into(),
            });
        }
        // By name, not by key: keys are random, the list is read by people.
        let mut names: Vec<String> = a
            .into_iter()
            .flat_map(|a| a.recipients.iter().map(|k| name(k)))
            .collect();
        names.sort();
        out.extend(names);
        out
    };
    let began = |epoch: u32| -> Option<String> {
        log.iter()
            .find(|e| e["kind"] == "cycled" && e["epoch"] == epoch)
            .and_then(|e| e["time"].as_str().map(String::from))
    };
    let removed: Vec<(String, String)> = log
        .iter()
        .filter(|e| e["kind"] == "recipients")
        .flat_map(|e| {
            let at = e["time"].as_str().unwrap_or_default().to_string();
            e["removed"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(move |x| Some((x["key"].as_str()?.to_string(), at.clone())))
        })
        .collect();
    let could = |epoch: u32, now: &[String]| -> Vec<(String, String)> {
        let from = began(epoch);
        let mut out: Vec<(String, String)> = Vec::new();
        for (k, at) in &removed {
            let n = name(k);
            if now.contains(&n) || out.iter().any(|(x, _)| *x == n) {
                continue;
            }
            if from.as_deref().is_none_or(|f| at.as_str() >= f) {
                out.push((n, at.clone()));
            }
        }
        out
    };
    let mut out = Vec::new();
    let now = opens(r.passphrase.as_ref(), r.age.as_ref());
    out.push(Holders {
        epoch: r.epoch,
        id: r.id.clone(),
        current: true,
        could: could(r.epoch, &now),
        opens: now,
    });
    for e in r.earlier.iter().rev() {
        let now = opens(e.passphrase.as_ref(), e.age.as_ref());
        out.push(Holders {
            epoch: e.epoch,
            id: e.id.clone(),
            current: false,
            could: could(e.epoch, &now),
            opens: now,
        });
    }
    Ok(out)
}

/// The X25519 key pair a master derives to be sealed to (R-166): its
/// secret and its public half.
fn seal_pair(k: &Key) -> ([u8; 32], [u8; 32]) {
    let secret = crate::secrets::derived(k, "dform seal key");
    let public = curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(secret).to_bytes();
    (secret, public)
}

/// The public key a deployment's `state.master` publishes, when it has
/// one.
pub fn public_of(store: &dyn crate::store::Store) -> Result<Option<[u8; 32]>> {
    let Some((r, _)) = load_record(store)? else {
        return Ok(None);
    };
    Ok(unhex(&r.public).and_then(|b| b.try_into().ok()))
}

/// The key a seal of `ephemeral`'s to `public` is under, from their shared
/// secret.
fn sealing_key(shared: &[u8; 32], ephemeral: &[u8; 32], public: &[u8; 32]) -> [u8; 32] {
    let mut salt = ephemeral.to_vec();
    salt.extend_from_slice(public);
    crate::secrets::hkdf(&salt, shared, b"dform sealed output", 32)
        .try_into()
        .expect("32 bytes")
}

/// `plain` sealed to the holder of `public`'s master for `label` (what it
/// is: another label's seal does not open as this one): an ephemeral
/// X25519 key's public half, a nonce, and XChaCha20-Poly1305 under the
/// key their shared secret gives; base64. What a stack publishes of a
/// secret output for each stack that reads it (R-166).
///
/// With `seed` (a key the producer's master derives) the ephemeral key and
/// the nonce derive from it, the reader's key, the label and the value:
/// the same value sealed to the same reader is the same seal, so
/// outputs.json does not change at every apply of the producer (a
/// reader's plan file stays fresh), and a new value, or a reader's new
/// master, a new one. Without, they are random.
pub fn seal_to(
    public: &[u8; 32],
    label: &str,
    plain: &[u8],
    seed: Option<&[u8; 32]>,
) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    use curve25519_dalek::montgomery::MontgomeryPoint;
    let (eph, nonce) = match seed {
        Some(seed) => {
            use sha2::Digest;
            let mut info = b"dform seal of ".to_vec();
            info.extend_from_slice(public);
            info.extend_from_slice(&sha2::Sha256::digest(label.as_bytes()));
            info.extend_from_slice(&sha2::Sha256::digest(plain));
            let b = crate::secrets::hkdf(b"dform sealed output", seed, &info, 32 + 24);
            let eph: [u8; 32] = b[..32].try_into().expect("32 bytes");
            let nonce: [u8; 24] = b[32..].try_into().expect("24 bytes");
            (eph, nonce)
        }
        None => (
            random_bytes::<32>("a seal's ephemeral key")?,
            random_bytes::<24>("a seal's nonce")?,
        ),
    };
    let eph_public = MontgomeryPoint::mul_base_clamped(eph).to_bytes();
    let shared = MontgomeryPoint(*public).mul_clamped(eph).to_bytes();
    let ct = cipher(&sealing_key(&shared, &eph_public, public))
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plain,
                aad: label.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("{label}: seal"))?;
    let mut b = eph_public.to_vec();
    b.extend_from_slice(&nonce);
    b.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(b))
}

/// What [`seal_to`] sealed for `label` to the master `k`; an error when it
/// was sealed to another, for another label, or altered.
pub fn open_sealed(k: &Key, label: &str, sealed: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    use curve25519_dalek::montgomery::MontgomeryPoint;
    let b = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .map_err(|e| anyhow::anyhow!("{label}: the seal is not base64: {e}"))?;
    if b.len() < 32 + 24 + 16 {
        bail!("{label}: the seal is too short");
    }
    let (eph_public, rest) = b.split_at(32);
    let (nonce, ct) = rest.split_at(24);
    let eph_public: [u8; 32] = eph_public.try_into().expect("32 bytes");
    let (secret, public) = seal_pair(k);
    let shared = MontgomeryPoint(eph_public).mul_clamped(secret).to_bytes();
    cipher(&sealing_key(&shared, &eph_public, &public))
        .decrypt(
            nonce.into(),
            Payload {
                msg: ct,
                aad: label.as_bytes(),
            },
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "{label}: does not open with this deployment's master: sealed to another"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seal_opens_with_its_readers_master_and_label_only() {
        let a = Key::from([1; 32]);
        let b = Key::from([2; 32]);
        let sealed = seal_to(&seal_pair(&a).1, "p#kc to a", b"kubeconfig", None).unwrap();
        assert_eq!(
            open_sealed(&a, "p#kc to a", &sealed).unwrap(),
            b"kubeconfig"
        );
        assert!(open_sealed(&b, "p#kc to a", &sealed).is_err());
        assert!(open_sealed(&a, "p#kc to b", &sealed).is_err());
        // Seeded: the same value to the same reader, the same seal; any
        // other value or reader, another; each opens as before.
        let seed = [7u8; 32];
        let to = |k: &Key, label: &str, v: &[u8]| {
            seal_to(&seal_pair(k).1, label, v, Some(&seed)).unwrap()
        };
        let one = to(&a, "p#kc to a", b"kubeconfig");
        assert_eq!(one, to(&a, "p#kc to a", b"kubeconfig"));
        assert_ne!(one, to(&a, "p#kc to a", b"kubeconfig 2"));
        assert_ne!(one, to(&b, "p#kc to a", b"kubeconfig"));
        assert_ne!(one, to(&a, "p#kc to b", b"kubeconfig"));
        assert_eq!(open_sealed(&a, "p#kc to a", &one).unwrap(), b"kubeconfig");
        assert!(open_sealed(&b, "p#kc to a", &one).is_err());
    }

    #[test]
    fn a_passphrase_opens_its_seal_only() {
        let k = Key::from([3; 32]);
        let id = key_id(&k);
        // `seal`'s, at a small cost for a unit test.
        let s = Sealed {
            kdf: "scrypt".into(),
            log_n: 10,
            r: 8,
            p: 1,
            salt: hex(&[5; 16]),
            sealed: String::new(),
        };
        let s = {
            use base64::Engine;
            use chacha20poly1305::aead::{Aead, Payload};
            let nonce = [9u8; 24];
            let ct = cipher(&mixed(b"pass", &s).unwrap())
                .encrypt(
                    (&nonce).into(),
                    Payload {
                        msg: &k.bytes(),
                        aad: id.as_bytes(),
                    },
                )
                .unwrap();
            let mut b = nonce.to_vec();
            b.extend_from_slice(&ct);
            Sealed {
                sealed: base64::engine::general_purpose::STANDARD.encode(b),
                ..s
            }
        };
        assert_eq!(open(&s, &id, b"pass").unwrap().unwrap().bytes(), k.bytes());
        assert!(open(&s, &id, b"other").unwrap().is_none());
        assert!(open(&s, "another id", b"pass").unwrap().is_none());
    }
}
