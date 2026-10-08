//! Given secrets (R-108): what a human types for a deployment (an admin
//! password, an API token another team issued), kept in the repository
//! in a file per deployment, sealed, and read by `set from
//! secrets.decode(io.read("secrets/${env}.json"))` into the deployment's
//! `secret(T)` inputs.
//!
//! The file is SOPS's JSON: each value `ENC[AES256_GCM,data:..,iv:..,
//! tag:..,type:str]` under a data key, its path the additional data
//! (`db:password:`), a MAC over every value in order, and the data key
//! sealed (an armored age file) to each recipient in the `sops`
//! block's `age` list. So `sops -d`, and fnox through it, open it with
//! an operator's own age identity. The recipients are the deployment's
//! (`[secrets] recipients`, R-164), and its own key where a passphrase or
//! the key file opens its master: an age identity the master derives
//! ([`identity_of`]), so whoever opens the master opens the file and no
//! one else does. The block's `dform` entry says which recipient that is
//! and, per value, its generation and when and by whom it was set, which
//! a run without the master reads its stand-in from ([`standin`]).
//!
//! `dform secrets set` keeps every other value's ciphertext as it is (the
//! data key is kept), so a plan without the master sees exactly the
//! value that changed; a recipient removed is a new data key, every value
//! sealed again (SOPS's `rotate`).

use super::Key;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The metadata's key in the document.
pub const META: &str = "sops";

/// The SOPS version a file written here says it is: the format of 3.x.
const VERSION: &str = "3.9.4";

/// The suffix SOPS leaves a key in the clear by; dform gives none.
const UNENCRYPTED: &str = "_unencrypted";

/// A given-secrets file.
#[derive(Debug, Clone, Default)]
pub struct File {
    /// Each value in document order.
    pub leaves: Vec<Leaf>,
    pub meta: Meta,
}

/// A value of the file: its path's segments, what the file holds, and the
/// line it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    pub path: Vec<String>,
    pub raw: Raw,
    pub line: Option<usize>,
}

impl Leaf {
    /// Its dotted path, as an input is named.
    pub fn name(&self) -> String {
        self.path.join(".")
    }
}

/// A value as the file holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    /// `ENC[AES256_GCM,..]`, or SOPS's "" for an empty value.
    Sealed(String),
    /// Anything else: a value in the clear.
    Plain(String),
}

/// The `sops` block.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    #[serde(default, deserialize_with = "null_is_empty")]
    pub age: Vec<AgeKey>,
    #[serde(default)]
    pub lastmodified: String,
    #[serde(default)]
    pub mac: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unencrypted_suffix: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dform: Option<Dform>,
    /// SOPS's other key sources (`kms`, `pgp`, ..), kept as they are.
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

fn null_is_empty<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(
    d: D,
) -> std::result::Result<Vec<T>, D::Error> {
    use serde::Deserialize;
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// The data key sealed to one recipient: an armored age file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgeKey {
    pub recipient: String,
    pub enc: String,
}

/// What dform keeps beside SOPS's metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dform {
    /// The recipient that is the deployment's own key ([`identity_of`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_key: Option<String>,
    /// Each value's record, by its dotted path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub given: BTreeMap<String, Given>,
}

/// When a value was set, by whom, and how many times.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Given {
    pub generation: u32,
    pub at: String,
    pub by: String,
}

impl File {
    /// The record of the value at `path`.
    pub fn given(&self, path: &str) -> Option<&Given> {
        self.meta.dform.as_ref()?.given.get(path)
    }

    /// The recipients the data key is sealed to.
    pub fn recipients(&self) -> Vec<String> {
        self.meta.age.iter().map(|a| a.recipient.clone()).collect()
    }

    /// The recipient that is the deployment's own key, if it is sealed to
    /// it.
    pub fn stack_key(&self) -> Option<&str> {
        self.meta.dform.as_ref()?.stack_key.as_deref()
    }

    /// The other key sources SOPS has sealed the data key to (a KMS key,
    /// a PGP key): what dform cannot seal a new data key to.
    fn foreign(&self) -> Vec<&str> {
        self.meta
            .other
            .iter()
            .filter(|(_, v)| match v {
                serde_json::Value::Array(xs) => !xs.is_empty(),
                serde_json::Value::Null => false,
                _ => true,
            })
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "shamir_threshold" | "encrypted_suffix" | "encrypted_regex"
                )
            })
            .map(|(k, _)| k.as_str())
            .collect()
    }
}

/// The file `text` (`shown` as messages name it): JSON, an object of
/// values (nested by path) and the `sops` block.
pub fn parse(text: &str, shown: &str) -> Result<File> {
    let j: serde_json::Value =
        serde_json::from_str(text).with_context(|| format!("{shown}: not JSON"))?;
    let serde_json::Value::Object(top) = &j else {
        bail!("{shown}: a file of given secrets is a JSON object");
    };
    let meta: Meta = match top.get(META) {
        Some(m) => serde_json::from_value(m.clone())
            .with_context(|| format!("{shown}: its `{META}` block"))?,
        None => bail!(
            "{shown}: no `{META}` block: not a file of sealed values (`dform secrets set` writes \
             one)"
        ),
    };
    // The values in the file's order, which the MAC is over: YAML's
    // mapping keeps it, and a JSON document is a YAML one.
    let ordered: serde_yaml::Value =
        serde_yaml::from_str(text).with_context(|| format!("{shown}: not JSON"))?;
    let serde_yaml::Value::Mapping(m) = ordered else {
        bail!("{shown}: a file of given secrets is a JSON object");
    };
    let mut leaves = Vec::new();
    for (k, v) in m {
        let Some(k) = k.as_str() else { continue };
        if k == META {
            continue;
        }
        walk(&mut leaves, vec![k.to_string()], v, shown)?;
    }
    // Each value's line: its top-level key's value where the document
    // has it, then each key under it, in order.
    let raw: BTreeMap<String, &serde_json::value::RawValue> =
        serde_json::from_str(text).with_context(|| format!("{shown}: not JSON"))?;
    let mut from: BTreeMap<&str, usize> = BTreeMap::new();
    for l in &mut leaves {
        let Some(r) = raw.get(&l.path[0]) else {
            continue;
        };
        let start = r.get().as_ptr() as usize - text.as_ptr() as usize;
        let end = start + r.get().len();
        let mut at = *from.get(l.path[0].as_str()).unwrap_or(&start);
        let mut found = true;
        for seg in &l.path[1..] {
            let needle = serde_json::to_string(seg).unwrap_or_default();
            match text[at..end].find(&needle) {
                Some(i) => at += i + needle.len(),
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            l.line = Some(text[..at].matches('\n').count() + 1);
            if let Some(k) = raw.keys().find(|k| **k == l.path[0]) {
                from.insert(k.as_str(), at);
            }
        }
    }
    Ok(File { leaves, meta })
}

fn walk(out: &mut Vec<Leaf>, path: Vec<String>, v: serde_yaml::Value, shown: &str) -> Result<()> {
    use serde_yaml::Value as Y;
    let raw = match v {
        Y::Mapping(m) => {
            for (k, v) in m {
                let k = match k {
                    Y::String(s) => s,
                    k => serde_yaml::to_string(&k)?.trim().to_string(),
                };
                let mut p = path.clone();
                p.push(k);
                walk(out, p, v, shown)?;
            }
            return Ok(());
        }
        Y::String(s) if s.is_empty() || s.starts_with("ENC[") => Raw::Sealed(s),
        Y::String(s) => Raw::Plain(s),
        Y::Number(n) => Raw::Plain(n.to_string()),
        Y::Bool(b) => Raw::Plain(b.to_string()),
        _ => bail!(
            "{shown}: {}: a given secret is a string, a number or a bool",
            path.join(".")
        ),
    };
    out.push(Leaf {
        path,
        raw,
        line: None,
    });
    Ok(())
}

/// A value in the clear: SOPS's types.
#[derive(Debug, Clone, PartialEq)]
pub enum Plain {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Plain {
    /// SOPS's type word, and its bytes (`ToBytes`: a bool is `True`).
    fn typed(&self) -> (&'static str, Vec<u8>) {
        match self {
            Plain::Str(s) => ("str", s.clone().into_bytes()),
            Plain::Int(i) => ("int", i.to_string().into_bytes()),
            Plain::Float(f) => ("float", float_text(*f).into_bytes()),
            Plain::Bool(true) => ("bool", b"True".to_vec()),
            Plain::Bool(false) => ("bool", b"False".to_vec()),
        }
    }

    /// As a program reads it.
    pub fn value(&self) -> crate::value::Value {
        use crate::value::Value;
        match self {
            Plain::Str(s) => Value::Str(s.clone()),
            Plain::Int(i) => Value::Int(*i),
            Plain::Float(f) => {
                crate::value::Float::new(*f).map_or(Value::Str(f.to_string()), Value::Float)
            }
            Plain::Bool(b) => Value::Bool(*b),
        }
    }
}

/// Go's `strconv.FormatFloat(f, 'f', -1, 64)`: the shortest digits, no
/// exponent, as Rust's own display of a float.
fn float_text(f: f64) -> String {
    format!("{f}")
}

/// The additional data a value at `path` is sealed with: SOPS's
/// `a:b:`.
fn aad(path: &[String]) -> String {
    let mut s = path.join(":");
    s.push(':');
    s
}

type Gcm = aes_gcm::AesGcm<aes_gcm::aes::Aes256, aes_gcm::aead::consts::U32>;

fn gcm(key: &[u8; 32]) -> Gcm {
    use aes_gcm::KeyInit;
    Gcm::new_from_slice(key).expect("a 32-byte key")
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// `plain` sealed under the data key `key` with `aad`: SOPS's
/// `ENC[AES256_GCM,data:..,iv:..,tag:..,type:T]`, a 32-byte IV; an empty
/// string is "", as SOPS leaves it.
fn encrypt(key: &[u8; 32], plain: &Plain, aad: &str) -> Result<String> {
    use aes_gcm::aead::{Aead, Payload};
    use base64::Engine;
    let (ty, bytes) = plain.typed();
    if bytes.is_empty() {
        return Ok(String::new());
    }
    let iv = super::random_bytes::<32>("a given secret's IV")?;
    let out = gcm(key)
        .encrypt(
            (&iv).into(),
            Payload {
                msg: &bytes,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("seal a given secret"))?;
    let (data, tag) = out.split_at(out.len() - 16);
    Ok(format!(
        "ENC[AES256_GCM,data:{},iv:{},tag:{},type:{ty}]",
        b64().encode(data),
        b64().encode(iv),
        b64().encode(tag),
    ))
}

/// What [`encrypt`] sealed; an error naming `what` when it is altered,
/// sealed under another key or at another path.
fn decrypt(key: &[u8; 32], sealed: &str, aad: &str, what: &str) -> Result<Plain> {
    use aes_gcm::aead::{Aead, Payload};
    use base64::Engine;
    if sealed.is_empty() {
        return Ok(Plain::Str(String::new()));
    }
    let bad =
        || anyhow!("{what}: not a sealed value (ENC[AES256_GCM,data:..,iv:..,tag:..,type:..])");
    let body = sealed
        .strip_prefix("ENC[AES256_GCM,")
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(bad)?;
    let mut parts: BTreeMap<&str, &str> = BTreeMap::new();
    for p in body.split(',') {
        let (k, v) = p.split_once(':').ok_or_else(bad)?;
        parts.insert(k, v);
    }
    let get = |k: &str| -> Result<Vec<u8>> {
        b64()
            .decode(parts.get(k).ok_or_else(bad)?)
            .map_err(|_| bad())
    };
    let (mut data, iv, tag) = (get("data")?, get("iv")?, get("tag")?);
    let ty = parts.get("type").copied().ok_or_else(bad)?;
    if iv.len() != 32 || tag.len() != 16 {
        bail!("{what}: a sealed value's IV is 32 bytes and its tag 16");
    }
    data.extend_from_slice(&tag);
    let iv: [u8; 32] = iv.try_into().expect("32 bytes");
    let plain = gcm(key)
        .decrypt(
            (&iv).into(),
            Payload {
                msg: &data,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| {
            anyhow!("{what}: does not open with the file's data key: it was altered or moved")
        })?;
    let text = String::from_utf8(plain).map_err(|_| anyhow!("{what}: not UTF-8 text"))?;
    Ok(match ty {
        "str" => Plain::Str(text),
        "int" => Plain::Int(text.parse().map_err(|_| anyhow!("{what}: not an int"))?),
        "float" => Plain::Float(
            text.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .ok_or_else(|| anyhow!("{what}: not a float"))?,
        ),
        "bool" => Plain::Bool(match text.to_ascii_lowercase().as_str() {
            "true" | "1" | "t" => true,
            "false" | "0" | "f" => false,
            _ => bail!("{what}: not a bool"),
        }),
        t => bail!("{what}: a value of SOPS's type {t}, which dform does not read"),
    })
}

/// The MAC of `values` in order: SHA-512 of their bytes, upper-case hex.
fn mac_of<'a>(values: impl IntoIterator<Item = &'a Plain>) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha512::new();
    for v in values {
        h.update(v.typed().1);
    }
    h.finalize().iter().map(|b| format!("{b:02X}")).collect()
}

/// The age identity a deployment's master derives (its digest key, the
/// first epoch's, so a `secrets cycle` keeps it): the file's recipient for
/// whoever opens the master by the passphrase or the key file.
pub fn identity_of(digest: &Key) -> age::x25519::Identity {
    use bech32::ToBase32;
    let bytes = crate::secrets::derived(digest, "dform given secrets age identity");
    bech32::encode(
        "age-secret-key-",
        bytes.to_base32(),
        bech32::Variant::Bech32,
    )
    .expect("a valid prefix")
    .to_uppercase()
    .parse()
    .expect("32 bytes are an identity")
}

/// The deployment's own recipient, `age1..`.
pub fn stack_recipient(digest: &Key) -> String {
    identity_of(digest).to_public().to_string()
}

/// Whether a file of `mixing`'s deployment is sealed to its master's own
/// key too: where a passphrase or the key file opens the master, so that
/// whoever opens the master opens the file.
pub fn to_master(mixing: &super::Mixing) -> bool {
    mixing.passphrase.is_some() || mixing.recipients.is_empty()
}

/// Who opens `f`, as messages say it: `sealed to alice, bob and the
/// deployment's master`, `names` naming a recipient.
pub fn sealed_to(f: &File, names: &dyn Fn(&str) -> String) -> String {
    let mut to: Vec<String> = f
        .recipients()
        .iter()
        .filter(|r| f.stack_key() != Some(r.as_str()))
        .map(|r| names(r))
        .collect();
    to.sort();
    if f.stack_key().is_some() {
        to.push("the deployment's master".into());
    }
    match to.split_last() {
        None => "sealed to no one".into(),
        Some((last, [])) => format!("sealed to {last}"),
        Some((last, rest)) => format!("sealed to {} and {last}", rest.join(", ")),
    }
}

/// A value given for `name`, read as its input's type `ty` (the type
/// inside `secret(..)`): an int, a float or a bool by its text, anything
/// else a string.
pub fn typed(name: &str, ty: &crate::ast::TypeExpr, text: String) -> Result<Plain> {
    let crate::ast::TypeExpr::Name(n) = ty else {
        return Ok(Plain::Str(text));
    };
    let bad = |what: &str| anyhow!("{name} is secret({n}): {what}");
    Ok(match n.as_str() {
        "int" => Plain::Int(text.trim().parse().map_err(|_| bad("not an int"))?),
        "float" => Plain::Float(
            text.trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .ok_or_else(|| bad("not a float"))?,
        ),
        "bool" => Plain::Bool(match text.trim() {
            "true" => true,
            "false" => false,
            _ => return Err(bad("true or false")),
        }),
        _ => Plain::Str(text),
    })
}

/// The value of `what`: stdin's, its one last line break dropped, else
/// asked on the terminal, not echoed. Never an argument: argv is readable
/// by every user of the host.
pub fn ask(what: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read, Write};
    let text = match std::io::stdin().is_terminal() {
        false => {
            let mut b = Vec::new();
            std::io::stdin().read_to_end(&mut b)?;
            let t = String::from_utf8(b).map_err(|_| anyhow!("{what}: stdin is not UTF-8 text"))?;
            let t = t.strip_suffix('\n').unwrap_or(&t);
            t.strip_suffix('\r').unwrap_or(t).to_string()
        }
        true => {
            let tty = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("open the terminal to ask the value")?;
            let mut out = tty.try_clone()?;
            write!(out, "{what}: ")?;
            out.flush()?;
            let echo = super::Echo::off(&tty);
            let mut line = String::new();
            std::io::BufReader::new(&tty).read_line(&mut line)?;
            drop(echo);
            writeln!(out)?;
            line.trim_end_matches(['\r', '\n']).to_string()
        }
    };
    if text.is_empty() {
        bail!("{what}: no value was given (`dform secrets unset` removes one)");
    }
    Ok(text)
}

/// The data key, opened by one of `identities`; `None` when the file is
/// sealed to none of them.
pub fn data_key(
    f: &File,
    identities: &[(age::x25519::Identity, String)],
) -> Result<Option<[u8; 32]>> {
    use std::io::Read;
    for a in &f.meta.age {
        let r = age::armor::ArmoredReader::new(a.enc.as_bytes());
        let d = age::Decryptor::new(r).map_err(|e| {
            anyhow!(
                "the data key sealed to {}: {e}",
                super::short_key(&a.recipient)
            )
        })?;
        let mut r = match d.decrypt(identities.iter().map(|(i, _)| i as &dyn age::Identity)) {
            Ok(r) => r,
            Err(age::DecryptError::NoMatchingKeys) => continue,
            Err(e) => bail!(
                "the data key sealed to {}: {e}",
                super::short_key(&a.recipient)
            ),
        };
        let mut plain = Vec::new();
        r.read_to_end(&mut plain)?;
        return Ok(Some(
            plain
                .try_into()
                .map_err(|_| anyhow!("the data key is not 32 bytes"))?,
        ));
    }
    Ok(None)
}

/// Every value of `f`, opened with its data key `key`, the MAC checked:
/// by dotted path, in order. A value in the clear is an error naming it.
pub fn open(f: &File, key: &[u8; 32], shown: &str) -> Result<Vec<(Leaf, Plain)>> {
    let mut out = Vec::new();
    for l in &f.leaves {
        let at = match l.line {
            Some(n) => format!("{shown}:{n}"),
            None => shown.to_string(),
        };
        let sealed = match &l.raw {
            Raw::Sealed(s) => s,
            Raw::Plain(_) => bail!(
                "{at}: {} is in the clear: a given secret is sealed (`dform secrets set` seals it)",
                l.name()
            ),
        };
        let p = decrypt(key, sealed, &aad(&l.path), &format!("{at}: {}", l.name()))?;
        out.push((l.clone(), p));
    }
    let want = decrypt(
        key,
        &f.meta.mac,
        &f.meta.lastmodified,
        &format!("{shown}: its MAC"),
    )?;
    if want != Plain::Str(mac_of(out.iter().map(|(_, p)| p))) {
        bail!("{shown}: its MAC is not its values': a value was altered, added or removed");
    }
    Ok(out)
}

/// The data key sealed to `recipient` (`age1..`): an armored age file.
fn seal_key(key: &[u8; 32], recipient: &str) -> Result<AgeKey> {
    use std::io::Write;
    let r: age::x25519::Recipient = recipient
        .parse()
        .map_err(|e| anyhow!("{recipient:?} is not an age recipient: {e}"))?;
    let enc = age::Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient))
        .map_err(|e| anyhow!("seal the data key to {}: {e}", super::short_key(recipient)))?;
    let mut out = Vec::new();
    let armor = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)?;
    let mut w = enc.wrap_output(armor)?;
    w.write_all(key)?;
    w.finish()?.finish()?;
    Ok(AgeKey {
        recipient: recipient.to_string(),
        enc: String::from_utf8(out).expect("armor is text"),
    })
}

/// How the file is to be sealed: to `recipients` (`age1..`, each
/// recipient of `[secrets]`, and the stack's own key, `stack_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct To {
    pub recipients: Vec<String>,
    pub stack_key: Option<String>,
}

impl To {
    fn all(&self) -> Vec<String> {
        let mut all = self.recipients.clone();
        all.extend(self.stack_key.clone());
        all.sort();
        all.dedup();
        all
    }
}

/// `f` (opened: `values`, under `key`; a new file has none) with the value
/// at `path` set to `plain` (`None`: removed), sealed `to`, by `who` at
/// `now`. A value not changed keeps its ciphertext, and the data key is
/// kept unless a recipient was removed: then a new one seals every value
/// again.
pub fn with(
    f: &File,
    (values, key): (&[(Leaf, Plain)], Option<[u8; 32]>),
    path: &str,
    plain: Option<Plain>,
    to: &To,
    (who, now): (&str, &str),
) -> Result<File> {
    let all = to.all();
    if all.is_empty() {
        bail!("there is no one to seal it to: no recipient, and no key of the deployment's");
    }
    let removed = f.recipients().iter().any(|r| !all.contains(r));
    let fresh = key.is_none() || removed;
    if fresh && !f.foreign().is_empty() && !f.leaves.is_empty() {
        bail!(
            "its data key is also sealed to {} (SOPS's), which dform cannot seal a new one to: \
             remove the recipient with sops, or those keys",
            f.foreign().join(", ")
        );
    }
    let key = match (key, fresh) {
        (Some(k), false) => k,
        _ => super::random_bytes::<32>("a data key")?,
    };
    let segs: Vec<String> = path.split('.').map(String::from).collect();
    // The values after the change, by path.
    let mut next: BTreeMap<Vec<String>, (Plain, Option<String>)> = values
        .iter()
        .map(|(l, p)| {
            let kept = match (&l.raw, fresh) {
                (Raw::Sealed(s), false) => Some(s.clone()),
                _ => None,
            };
            (l.path.clone(), (p.clone(), kept))
        })
        .collect();
    let mut dform = f.meta.dform.clone().unwrap_or_default();
    match plain {
        Some(p) => {
            if let Some(clash) = next
                .keys()
                .find(|k| **k != segs && (k.starts_with(&segs) || segs.starts_with(k)))
            {
                bail!(
                    "{path} and {} would both be values, one inside the other",
                    clash.join(".")
                );
            }
            next.insert(segs.clone(), (p, None));
            let generation = dform.given.get(path).map_or(1, |g| g.generation + 1);
            dform.given.insert(
                path.to_string(),
                Given {
                    generation,
                    at: now.to_string(),
                    by: who.to_string(),
                },
            );
        }
        None => {
            next.remove(&segs);
            dform.given.remove(path);
        }
    }
    dform.stack_key = to.stack_key.clone();
    let mut leaves = Vec::new();
    for (p, (plain, kept)) in &next {
        let sealed = match kept {
            Some(s) => s.clone(),
            None => encrypt(&key, plain, &aad(p))?,
        };
        leaves.push(Leaf {
            path: p.clone(),
            raw: Raw::Sealed(sealed),
            line: None,
        });
    }
    let lastmodified = now.to_string();
    let mac = encrypt(
        &key,
        &Plain::Str(mac_of(next.values().map(|(p, _)| p))),
        &lastmodified,
    )?;
    let age = all
        .iter()
        .map(|r| match f.meta.age.iter().find(|a| &a.recipient == r) {
            Some(a) if !fresh => Ok(a.clone()),
            _ => seal_key(&key, r),
        })
        .collect::<Result<_>>()?;
    Ok(File {
        leaves,
        meta: Meta {
            age,
            lastmodified,
            mac,
            unencrypted_suffix: UNENCRYPTED.into(),
            version: VERSION.into(),
            dform: Some(dform),
            other: match fresh {
                true => BTreeMap::new(),
                false => f.meta.other.clone(),
            },
        },
    })
}

/// `f` as the file holds it: pretty JSON, its values by path (sorted, as
/// its MAC is over them), then the `sops` block.
pub fn text(f: &File) -> Result<String> {
    let mut top = serde_json::Map::new();
    for l in &f.leaves {
        let Raw::Sealed(s) = &l.raw else {
            bail!("{} is in the clear", l.name());
        };
        let (last, parents) = l.path.split_last().expect("a path has a segment");
        let mut at = &mut top;
        for p in parents {
            at = at
                .entry(p.clone())
                .or_insert_with(|| serde_json::Value::Object(Default::default()))
                .as_object_mut()
                .ok_or_else(|| anyhow!("{} is inside a value", l.name()))?;
        }
        at.insert(last.clone(), serde_json::Value::String(s.clone()));
    }
    top.insert(META.into(), serde_json::to_value(&f.meta)?);
    let mut s = serde_json::to_string_pretty(&serde_json::Value::Object(top))?;
    s.push('\n');
    Ok(s)
}

/// What a run without the master reads in place of the value at `path`
/// of the file `shown` (and a run with it registers as the value's
/// stand-in, `secrets::standin`): a function of the file, the path and the
/// value's generation, public, so a leaf that holds it has the digest an
/// apply with the master recorded until `secrets set` sets it again.
pub fn standin(shown: &str, path: &str, generation: u32) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(format!("dform given secret\0{shown}\0{path}\0{generation}"));
    let hex: String = d.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("given-{hex}")
}

/// The run's key for given secrets: its master's digest key, when it holds
/// it; set before each evaluation.
static KEY: Mutex<Option<Key>> = Mutex::new(None);

/// The files of given secrets the last evaluation read.
static READ: Mutex<Vec<Read>> = Mutex::new(Vec::new());

/// A file of given secrets a run read.
#[derive(Debug, Clone)]
pub struct Read {
    /// As the program names it, and as rows name it.
    pub location: String,
    pub shown: String,
    /// The file on disk, for `secrets set`: a project file's.
    pub path: Option<std::path::PathBuf>,
    /// The file, `None` when it is not there yet.
    pub file: Option<File>,
}

/// Before an evaluation: the master's digest key (`custody::Master::digest`)
/// when the run holds it; the files read are forgotten.
pub fn set_key(k: Option<Key>) {
    *KEY.lock().unwrap_or_else(|e| e.into_inner()) = k;
    READ.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

fn key() -> Option<Key> {
    KEY.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The files of given secrets the last evaluation read.
pub fn reads() -> Vec<Read> {
    READ.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A file a run read, recorded.
pub fn note(r: Read) {
    let mut all = READ.lock().unwrap_or_else(|e| e.into_inner());
    if !all.iter().any(|x| x.location == r.location) {
        all.push(r);
    }
}

/// The identities this run opens a file of given secrets with: the
/// operator's (`AGE_IDENTITY`, `age:NAME`), and the master's.
pub fn identities() -> Result<Vec<(age::x25519::Identity, String)>> {
    let mut ids = super::identities()?;
    if let Some(k) = key() {
        ids.push((identity_of(&k), "the deployment's master".into()));
    }
    Ok(ids)
}

/// Why no identity of `ids` opens `f`, as a message ends.
pub fn why_not(
    f: &File,
    ids: &[(age::x25519::Identity, String)],
    names: &dyn Fn(&str) -> String,
) -> String {
    let to: Vec<String> = f
        .recipients()
        .iter()
        .map(|r| match f.stack_key() == Some(r.as_str()) {
            true => "the deployment's master".to_string(),
            false => names(r),
        })
        .collect();
    format!(
        "it is sealed to {}, and this run holds {}",
        match to.is_empty() {
            true => "no one".to_string(),
            false => to.join(", "),
        },
        match ids.is_empty() {
            true => "no age identity (AGE_IDENTITY, or a credential age:NAME)".to_string(),
            false => format!(
                "none of them ({} tried)",
                ids.iter()
                    .map(|(_, f)| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    )
}

/// The rows of the file `f` read at `shown` (`set from
/// secrets.decode(..)`): each value's line, dotted path and value; in a
/// run without the master, each value's stand-in.
pub fn rows(f: &File, shown: &str) -> Result<Vec<(Option<usize>, String, crate::value::Value)>> {
    use crate::value::Value;
    let at = |l: &Leaf| match l.line {
        Some(n) => format!("{shown}:{n}"),
        None => shown.to_string(),
    };
    for l in &f.leaves {
        if let Raw::Plain(_) = l.raw {
            bail!(
                "{}: {} is in the clear: a given secret is sealed; `dform secrets set` seals it",
                at(l),
                l.name()
            );
        }
    }
    let mut out = Vec::new();
    if crate::secrets::standin::active() {
        // No master: each value by its stand-in, as any secret it derives.
        for l in &f.leaves {
            let name = l.name();
            let s = standin(shown, &name, f.given(&name).map_or(1, |g| g.generation));
            crate::secrets::standin::register(&s, &label(shown, &name), &s);
            out.push((l.line, name, Value::Str(s)));
        }
    } else {
        let ids = identities()?;
        let Some(key) = data_key(f, &ids)? else {
            bail!("{shown}: {}", why_not(f, &ids, &|r| super::short_key(r)));
        };
        for (l, p) in open(f, &key, shown)? {
            let name = l.name();
            if let Plain::Str(v) = &p {
                let s = standin(shown, &name, f.given(&name).map_or(1, |g| g.generation));
                crate::secrets::standin::register(v, &label(shown, &name), &s);
            }
            out.push((l.line, name, p.value()));
        }
    }
    Ok(out)
}

/// A given secret's label, as a plan names it without its value.
fn label(shown: &str, path: &str) -> String {
    format!("given {path} ({shown})")
}
