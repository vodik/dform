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
            key,
            random,
            source,
            ..Master::default()
        }
    }

    /// Refuse a master other than the one state was applied with
    /// (`applied`, its id), unless `--new-master` accepts it.
    pub fn check(&self, deployment: &str, applied: Option<&str>) -> Result<()> {
        let (Some(was), Some(now)) = (applied, self.id.as_deref()) else {
            return Ok(());
        };
        if was == now || self.accept {
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
    /// The master sealed under a key scrypt mixes from the passphrase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<Sealed>,
}

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
                            id,
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
    let of = Master::of(key, out.source.clone(), env);
    Ok(Master {
        id: of.id.or(out.id),
        key: of.key,
        random: of.random,
        source: of.source,
        ..out
    })
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
            id,
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
