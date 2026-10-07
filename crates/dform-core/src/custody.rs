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

/// A master id as messages print it: its first 12 hex digits.
pub fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
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
    /// The key is a plain key file the passphrase is to seal
    /// ([`seal_key_file`]).
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
            short(now),
            short(was),
        )
    }
}

/// How a deployment's master is kept: dform.toml's `[secrets]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Mixing {
    /// No `[secrets]`: the master is the key file `state.key` beside the
    /// state, on the machine's disk: a local backend's only.
    #[default]
    KeyFile,
    /// `[secrets] passphrase`: the backend holds the master sealed under a
    /// key scrypt mixes from the passphrase and a salt (`state.master`),
    /// never in the clear.
    Passphrase(Passphrase),
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
    /// The mixing a project's dform.toml names.
    pub fn of(manifest: Option<&crate::project::Manifest>) -> Result<Mixing> {
        match manifest.and_then(|m| m.secrets.passphrase.as_deref()) {
            Some(p) => Ok(Mixing::Passphrase(Passphrase::parse(p)?)),
            None => Ok(Mixing::KeyFile),
        }
    }
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
    /// Which epoch this master is (R-165): 1 until `dform secrets cycle`.
    #[serde(default = "first_epoch", skip_serializing_if = "is_first_epoch")]
    pub epoch: u32,
    /// The earlier epochs' masters a secret still derives from, each
    /// sealed under the passphrase; one no secret derives from is retired
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
    pub passphrase: Sealed,
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
    Ok(Some(Key::from_bytes(bytes)))
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
pub fn key_id(k: &Key) -> String {
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
/// master is read (a sealed one opened with the passphrase); one is made
/// only for a deployment never applied (`want.make`), or when
/// `--new-master` asks for it. A run that has no passphrase has no key:
/// `Master::without` says why.
pub fn resolve(
    store: &dyn crate::store::Store,
    deployment: &str,
    applied: &dyn Fn() -> Result<bool>,
    mixing: &Mixing,
    want: Want,
) -> Result<Master> {
    use crate::store::{Cond, KEY, MASTER};
    let env = std::env::var("RANDOM_MASTER")
        .ok()
        .filter(|m| !m.is_empty())
        .map(String::into_bytes);
    let record = load_record(store)?;
    let plain = Key::load(store)?;
    let missing = || {
        anyhow::anyhow!(
            "{deployment}: {} is missing, and the deployment was applied with it: every random.* \
             value and every secret digest derives from it. Restore it from a backup of the state \
             (state and key go together), or run with --new-master to make a new one and change \
             every derived secret on purpose",
            match mixing {
                Mixing::KeyFile => store.locate(KEY),
                Mixing::Passphrase(_) => store.locate(MASTER),
            }
        )
    };
    let fresh = || -> Result<Key> { Ok(Key::from_bytes(random_bytes::<32>("the master")?)) };
    let mut out = Master {
        accept: want.new_master,
        ..Master::default()
    };
    let key = match mixing {
        Mixing::KeyFile => {
            if let Some((r, _)) = record.as_ref().filter(|(r, _)| r.passphrase.is_some()) {
                bail!(
                    "{deployment}: {} holds its master sealed with a passphrase (id {}), and \
                     dform.toml names none: add `[secrets] passphrase = \"env:NAME\"` (or \
                     \"prompt\")",
                    store.locate(MASTER),
                    short(&r.id)
                );
            }
            out.source = format!("the key file {}", store.locate(KEY));
            match plain {
                Some(k) => {
                    if !store.local() {
                        static SAID: std::sync::Once = std::sync::Once::new();
                        SAID.call_once(|| {
                            eprintln!(
                                "warning: {deployment}'s master is the key file {}, in the bucket \
                                 beside the state: read access to the state is read access to \
                                 every derived secret. Set `[secrets] passphrase` in dform.toml; \
                                 the next apply seals it",
                                store.locate(KEY)
                            )
                        });
                    }
                    Some(k)
                }
                None if applied()? && !want.new_master => return Err(missing()),
                None if want.make || want.new_master => {
                    if !store.local() {
                        bail!(
                            "{deployment}: a new master would be the key file {}, in the bucket \
                             beside the state, where read access to the state is read access to \
                             every derived secret: set `[secrets] passphrase = \"env:NAME\"` (or \
                             \"prompt\") in dform.toml, and the bucket keeps it sealed",
                            store.locate(KEY)
                        );
                    }
                    let key = fresh()?;
                    match store
                        .put(KEY, &key.bytes(), &Cond::IfAbsent)
                        .with_context(|| format!("write the key {}", store.locate(KEY)))?
                    {
                        Some(_) => {
                            out.made = true;
                            Some(key)
                        }
                        // Made by another run meanwhile: that one is the key.
                        None => Some(Key::load(store)?.ok_or_else(|| {
                            anyhow::anyhow!("the key {}: gone", store.locate(KEY))
                        })?),
                    }
                }
                None => None,
            }
        }
        Mixing::Passphrase(from) => {
            let pass = from.read(deployment)?;
            out.source = format!("{} sealed with {}", store.locate(MASTER), from.describe());
            match (&record, plain) {
                (Some((r, _)), plain) if r.passphrase.is_some() => {
                    out.id = Some(r.id.clone());
                    out.epoch = r.epoch;
                    // The earlier epochs (R-165): their ids, and their keys
                    // with the passphrase.
                    for e in &r.earlier {
                        let key = match &pass {
                            Ok(p) => match open(&e.passphrase, &e.id, p)? {
                                Some(k) if key_id(&k) == e.id => Some(k),
                                _ => bail!(
                                    "{deployment}: {}'s epoch {} (id {}) does not open with the \
                                     passphrase from {}",
                                    store.locate(MASTER),
                                    e.epoch,
                                    short(&e.id),
                                    from.describe()
                                ),
                            },
                            Err(_) => None,
                        };
                        out.earlier.push(Epoch {
                            epoch: e.epoch,
                            id: e.id.clone(),
                            key,
                        });
                    }
                    // A key file a sealing left behind goes with the next
                    // apply ([`seal_key_file`]).
                    out.unsealed = plain.is_some();
                    let sealed = r.passphrase.as_ref().expect("matched");
                    match &pass {
                        Ok(p) => match open(sealed, &r.id, p)? {
                            Some(k) if key_id(&k) == r.id => Some(k),
                            Some(_) => bail!(
                                "{deployment}: {} opens to a master whose id is not its own: it \
                                 was altered",
                                store.locate(MASTER)
                            ),
                            None => bail!(
                                "{deployment}: the passphrase from {} does not open {} (id {}): \
                                 not the passphrase it was sealed with",
                                from.describe(),
                                store.locate(MASTER),
                                short(&r.id)
                            ),
                        },
                        Err(why) => {
                            out.without = Some(why.clone());
                            None
                        }
                    }
                }
                // Sealed by the next apply that has the passphrase.
                (_, Some(k)) => {
                    out.source = format!("the key file {}", store.locate(KEY));
                    out.unsealed = true;
                    Some(k)
                }
                (_, None) if applied()? && !want.new_master => return Err(missing()),
                (_, None) => match &pass {
                    Ok(p) if want.make || want.new_master => {
                        let key = fresh()?;
                        let id = key_id(&key);
                        let r = Record {
                            version: 1,
                            passphrase: Some(seal(&key, &id, p)?),
                            public: hex(&seal_pair(&key).1),
                            id,
                            epoch: 1,
                            earlier: Vec::new(),
                            digest: String::new(),
                        };
                        let cond = match &record {
                            Some((_, etag)) => Cond::IfMatch(etag.clone()),
                            None => Cond::IfAbsent,
                        };
                        match store
                            .put(MASTER, &record_bytes(&r), &cond)
                            .with_context(|| format!("write {}", store.locate(MASTER)))?
                        {
                            Some(_) => {
                                out.made = true;
                                Some(key)
                            }
                            None => bail!(
                                "{deployment}: {} was written by another run meanwhile: run \
                                 again",
                                store.locate(MASTER)
                            ),
                        }
                    }
                    Ok(_) => None,
                    Err(why) => {
                        out.without = Some(why.clone());
                        None
                    }
                },
            }
        }
    };
    if env.is_some() && (key.is_some() || out.id.is_some()) {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "warning: RANDOM_MASTER is set: random.* derive from it, not from {deployment}'s \
                 own master"
            )
        });
    }
    // The public key other stacks seal to (R-166), kept beside the master
    // by a run that writes: a key file's deployment gets a record of its
    // own, its id and public key only.
    if let (true, Some(k)) = (want.make || want.new_master, &key) {
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
    }
    let of = Master::of(key, out.source.clone(), env);
    // After a cycle the digest key is the first epoch's, sealed under the
    // current master.
    let digest = match (&record, &of.key) {
        (Some((r, _)), Some(k)) if !r.digest.is_empty() => {
            let b: [u8; 32] = crate::secrets::open(k, DIGEST_ROOT, &r.digest)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("{deployment}: the digest key is not 32 bytes"))?;
            Some(Key::from_bytes(b))
        }
        _ => of.digest,
    };
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

/// `dform secrets cycle` (R-165): a new master, epoch N+1, sealed under the
/// passphrase beside epoch N, which stays (sealed) while a secret derives
/// from it; the digest key carried over. Its epoch and id.
pub fn cycle(
    store: &dyn crate::store::Store,
    deployment: &str,
    master: &Master,
    mixing: &Mixing,
) -> Result<(u32, String)> {
    use crate::store::{Cond, MASTER};
    let Mixing::Passphrase(from) = mixing else {
        bail!(
            "{deployment}: its master is the key file {}: an epoch is kept sealed beside the \
             next, so cycling needs `[secrets] passphrase` in dform.toml (the next apply seals the \
             key file)",
            store.locate(crate::store::KEY)
        );
    };
    let (Some(_), Some(digest)) = (&master.key, &master.digest) else {
        bail!(
            "{deployment}: cycling seals a new master and needs the current one ({})",
            master.without.as_deref().unwrap_or("not held")
        );
    };
    let Some((r, etag)) = load_record(store)? else {
        bail!("{deployment}: {} is missing", store.locate(MASTER));
    };
    let Some(sealed) = r.passphrase.clone().filter(|_| !master.unsealed) else {
        bail!(
            "{deployment}: its master is not sealed yet: apply once with the passphrase, then \
             cycle"
        );
    };
    let pass = match from.read(deployment)? {
        Ok(p) => p,
        Err(why) => bail!("{deployment}: cycling needs the passphrase: {why}"),
    };
    let new = Key::from_bytes(random_bytes::<32>("the master")?);
    let id = key_id(&new);
    let mut earlier = r.earlier.clone();
    earlier.push(EarlierRecord {
        epoch: r.epoch,
        id: r.id.clone(),
        passphrase: sealed,
    });
    let next = Record {
        version: 1,
        passphrase: Some(seal(&new, &id, &pass)?),
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

/// Seal a master the backend keeps as a plain key file under the
/// passphrase (`mixing`), and remove the file: what the first apply that
/// has the passphrase does (R-164). Whether it did.
pub fn seal_key_file(
    store: &dyn crate::store::Store,
    master: &Master,
    mixing: &Mixing,
) -> Result<bool> {
    use crate::store::{Cond, KEY, MASTER};
    let (Mixing::Passphrase(from), true, Some(key)) = (mixing, master.unsealed, &master.key) else {
        return Ok(false);
    };
    let Ok(pass) = from.read("")? else {
        return Ok(false);
    };
    let id = key_id(key);
    let record = load_record(store)?;
    let sealed = record
        .as_ref()
        .is_some_and(|(r, _)| r.passphrase.is_some() && r.id == id);
    if !sealed {
        let r = Record {
            version: 1,
            passphrase: Some(seal(key, &id, &pass)?),
            public: hex(&seal_pair(key).1),
            id,
            epoch: 1,
            earlier: Vec::new(),
            digest: String::new(),
        };
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
                "{} was written by another run while the key file was being sealed: run again",
                store.locate(MASTER)
            );
        }
    }
    store.delete(KEY)?;
    Ok(true)
}

/// The X25519 key pair a master derives to be sealed to (R-166): its
/// secret and its public half.
pub fn seal_pair(k: &Key) -> ([u8; 32], [u8; 32]) {
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
pub fn seal_to(public: &[u8; 32], label: &str, plain: &[u8]) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    use curve25519_dalek::montgomery::MontgomeryPoint;
    let eph = random_bytes::<32>("a seal's ephemeral key")?;
    let eph_public = MontgomeryPoint::mul_base_clamped(eph).to_bytes();
    let shared = MontgomeryPoint(*public).mul_clamped(eph).to_bytes();
    let nonce = random_bytes::<24>("a seal's nonce")?;
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
        let a = Key::from_bytes([1; 32]);
        let b = Key::from_bytes([2; 32]);
        let sealed = seal_to(&seal_pair(&a).1, "p#kc to a", b"kubeconfig").unwrap();
        assert_eq!(
            open_sealed(&a, "p#kc to a", &sealed).unwrap(),
            b"kubeconfig"
        );
        assert!(open_sealed(&b, "p#kc to a", &sealed).is_err());
        assert!(open_sealed(&a, "p#kc to b", &sealed).is_err());
    }

    #[test]
    fn a_passphrase_opens_its_seal_only() {
        let k = Key::from_bytes([3; 32]);
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
