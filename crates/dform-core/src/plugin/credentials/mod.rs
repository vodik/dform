//! Credentials by name (R-13b, R-45): a provider opens a credential
//! through the host (`dform:host/secrets`), and the host applies the value
//! to its own call (`dform:host/http`'s `auth`). The value is never
//! serialized to a provider.
//!
//! A name is `KIND:NAME`; the kind says how the host applies it:
//!
//! | kind         | the value                         | applied as                              |
//! |--------------|-----------------------------------|-----------------------------------------|
//! | `bearer`     | a token                           | `Authorization: Bearer TOKEN`           |
//! | `basic`      | `USER:PASSWORD`                   | `Authorization: Basic ..`               |
//! | `header`     | `NAME: VALUE`                     | that header                             |
//! | `tls`        | PEM: a certificate chain and key  | the TLS session's client certificate    |
//! | `kubeconfig` | a kubeconfig (its current context), or its parts ([`kubeconfig`]) | its user's token, client certificate or exec plugin's answer, its cluster's CA; its server is the credential's endpoint, its namespace the credential's |
//! | `ssh`        | an unencrypted OpenSSH private key | by the `ssh` provider (`use ssh { key = "NAME" }`), never to an HTTP call; only the operator's file ([`file`]) |
//!
//! Where a value comes from:
//!
//! - held for one provider ([`hold`]): a setting its handshake declares a
//!   credential (`kubeconfig`), as the program gives it (`use k8s {
//!   kubeconfig = cluster.kubeconfig }`, a secret revealed into the
//!   Configure, R-45), or, for a provider the program does not configure,
//!   the environment's (`KUBECONFIG`). The engine holds it at that
//!   Configure under a name of its own, `KIND:PROVIDER.N`, and the
//!   provider is configured with the name; only that provider opens it,
//!   and it is gone once its session is configured again or its link
//!   stops;
//! - else the operator's file `$XDG_CONFIG_HOME/dform/credentials/KIND/NAME`
//!   (or under `DFORM_CREDENTIALS`), as dform.toml grants it
//!   (`[providers.k8s] credentials = ["kubeconfig:prod"]`).

pub mod kubeconfig;

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes that are a credential's: never printed, zeroed when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Secret {
        Secret(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    fn text(&self, name: &str) -> Result<&str> {
        std::str::from_utf8(&self.0).map_err(|_| anyhow!("the credential {name} is not text"))
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // SAFETY: `b` is a valid, aligned &mut u8; a volatile write is
            // not elided as a dead store.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

/// What a credential's kind is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Bearer,
    Basic,
    Header,
    Tls,
    Kubeconfig,
    Ssh,
}

pub const KINDS: [&str; 6] = ["bearer", "basic", "header", "tls", "kubeconfig", "ssh"];

impl Kind {
    /// The kind `word` names, as a credential's name or a setting's
    /// declaration spells it.
    pub fn named(word: &str) -> Option<Kind> {
        Some(match word {
            "bearer" => Kind::Bearer,
            "basic" => Kind::Basic,
            "header" => Kind::Header,
            "tls" => Kind::Tls,
            "kubeconfig" => Kind::Kubeconfig,
            "ssh" => Kind::Ssh,
            _ => return None,
        })
    }

    /// Whether the environment has one of the kind (a kubeconfig's:
    /// `KUBECONFIG`), which a provider the program does not configure is
    /// given ([`Given::Environment`]).
    pub fn in_environment(self) -> bool {
        self == Kind::Kubeconfig
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Bearer => "bearer",
            Kind::Basic => "basic",
            Kind::Header => "header",
            Kind::Tls => "tls",
            Kind::Kubeconfig => "kubeconfig",
            Kind::Ssh => "ssh",
        }
    }
}

/// `KIND:NAME`, checked.
pub fn parse_name(name: &str) -> Result<(Kind, &str)> {
    let Some((kind, rest)) = name.split_once(':') else {
        bail!(
            "the credential {name:?} names no kind: write KIND:NAME, KIND one of {}",
            KINDS.join(", ")
        );
    };
    let Some(kind) = Kind::named(kind) else {
        bail!(
            "the credential {name:?} has the kind {kind:?}: expected one of {}",
            KINDS.join(", ")
        )
    };
    if rest.is_empty() || rest.contains('/') || rest.starts_with('.') {
        bail!("the credential {name:?}: its NAME must be a plain name");
    }
    Ok((kind, rest))
}

/// A credential as the host applies it: each part it has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credential {
    /// The name it was opened by.
    pub name: String,
    /// What it is for, when it says (a kubeconfig's server): not secret.
    pub endpoint: Option<String>,
    /// The namespace it defaults to, when it says (a kubeconfig context's):
    /// not secret.
    pub namespace: Option<String>,
    /// Headers to set (Authorization, or the `header` kind's).
    pub headers: Vec<(String, Secret)>,
    /// A client certificate chain and its key, PEM.
    pub client_cert: Option<(Secret, Secret)>,
    /// Certificates to trust for this credential's endpoint, PEM, beside
    /// the machine's.
    pub ca: Option<Vec<u8>>,
    /// Skip verifying the server (a kubeconfig's
    /// `insecure-skip-tls-verify`).
    pub insecure: bool,
}

impl Credential {
    fn named(name: &str) -> Credential {
        Credential {
            name: name.to_string(),
            ..Credential::default()
        }
    }

    fn bearer(&mut self, token: &str) {
        self.headers.push((
            "Authorization".into(),
            Secret::new(format!("Bearer {}", token.trim()).into_bytes()),
        ));
    }

    fn basic(&mut self, pair: &str) {
        use base64::Engine;
        let enc = base64::engine::general_purpose::STANDARD.encode(pair);
        self.headers.push((
            "Authorization".into(),
            Secret::new(format!("Basic {enc}").into_bytes()),
        ));
    }

    /// `value` applied as its kind says: one value, the operator's file or
    /// a setting.
    fn apply(&mut self, kind: Kind, value: &Secret, dir: Option<&Path>) -> Result<()> {
        let name = self.name.clone();
        match kind {
            Kind::Bearer => self.bearer(value.text(&name)?),
            Kind::Basic => {
                let pair = value.text(&name)?.trim();
                if !pair.contains(':') {
                    bail!("the credential {name} is not USER:PASSWORD");
                }
                self.basic(pair);
            }
            Kind::Header => {
                let line = value.text(&name)?.trim();
                let (h, val) = line
                    .split_once(':')
                    .ok_or_else(|| anyhow!("the credential {name} is not `NAME: VALUE`"))?;
                self.headers.push((
                    h.trim().to_string(),
                    Secret::new(val.trim().as_bytes().to_vec()),
                ));
            }
            Kind::Tls => {
                let pem = value.text(&name)?;
                if !pem.contains("PRIVATE KEY") || !pem.contains("CERTIFICATE") {
                    bail!("the credential {name} is not a PEM certificate chain and key");
                }
                self.client_cert = Some((value.clone(), value.clone()));
            }
            Kind::Kubeconfig => {
                let what = format!("the credential {name}");
                kubeconfig::Kubeconfig::parse(&what, value.text(&name)?, dir)?.apply(self)?
            }
            Kind::Ssh => bail!(
                "the credential {name} is an SSH key: the ssh provider uses it (`use ssh {{ key \
                 = .. }}`), never an HTTP call"
            ),
        }
        Ok(())
    }
}

/// What a held credential's value is ([`hold`]).
#[derive(Debug, Clone)]
pub enum Given {
    /// The settings of a provider's Configure that its handshake declares
    /// this kind, by setting: one is the value; several are a kind's
    /// parts (a kubeconfig's `host`, `ca`, `token`, ..,
    /// [`kubeconfig::Kubeconfig::of_parts`]).
    Settings(BTreeMap<String, Secret>),
    /// The environment's credential of the kind, read when it is opened:
    /// a kubeconfig's ([`kubeconfig::Kubeconfig::environment`]).
    Environment,
}

impl Given {
    /// The credential `name` of `kind` this gives, as the host applies it.
    fn load(&self, kind: Kind, name: &str) -> Result<Credential> {
        let mut c = Credential::named(name);
        match (self, kind) {
            (Given::Environment, Kind::Kubeconfig) => {
                kubeconfig::Kubeconfig::environment()?.apply(&mut c)?
            }
            (Given::Environment, _) => {
                bail!(
                    "a {} credential has no source in the environment",
                    kind.as_str()
                )
            }
            (Given::Settings(parts), Kind::Kubeconfig) => {
                kubeconfig::Kubeconfig::of_parts(name, parts)?.apply(&mut c)?
            }
            (Given::Settings(parts), _) => {
                let mut values = parts.values();
                let (Some(v), None) = (values.next(), values.next()) else {
                    bail!(
                        "a {} credential is one setting, not {}",
                        kind.as_str(),
                        parts.keys().cloned().collect::<Vec<_>>().join(", ")
                    );
                };
                c.apply(kind, v, None)?
            }
        }
        Ok(c)
    }
}

/// A credential held for a provider: its kind, the provider, its value.
struct Held {
    kind: Kind,
    to: String,
    given: Given,
}

/// The credentials held, by name ([`hold`]).
static HELD: Mutex<BTreeMap<String, Held>> = Mutex::new(BTreeMap::new());
/// The number the next held credential's name takes.
static NEXT: AtomicU64 = AtomicU64::new(1);

fn held_map() -> std::sync::MutexGuard<'static, BTreeMap<String, Held>> {
    // Each use is one insert, removal or lookup: a panic leaves it whole.
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// A credential held under its name until this is dropped.
#[derive(Debug)]
pub struct Holding(String);

impl Holding {
    /// The name the provider opens it by.
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        held_map().remove(&self.0);
    }
}

/// Hold `given` as a credential of `kind` for the provider `to` (its name
/// in dform.toml, as its host's grants say): in this process's memory,
/// under a name of its own, `KIND:TO.N`, which only `to` opens
/// ([`held`]), until the holding is dropped.
pub fn hold(kind: Kind, to: &str, given: Given) -> Holding {
    let name = format!(
        "{}:{to}.{}",
        kind.as_str(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    held_map().insert(
        name.clone(),
        Held {
            kind,
            to: to.to_string(),
            given,
        },
    );
    Holding(name)
}

/// The credential `name` as the host applies it, if it is held for the
/// provider `to` ([`hold`]); `None` when it is not one held for it. Its
/// error says why it does not load, never a value.
pub fn held(name: &str, to: &str) -> Option<Result<Credential>> {
    let (kind, given) = {
        let map = held_map();
        let h = map.get(name).filter(|h| h.to == to)?;
        (h.kind, h.given.clone())
    };
    Some(given.load(kind, name))
}

/// Where the operator's credential files are: `DFORM_CREDENTIALS`, else
/// `$XDG_CONFIG_HOME/dform/credentials`, else
/// `~/.config/dform/credentials`.
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DFORM_CREDENTIALS") {
        return Some(PathBuf::from(d));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("dform").join("credentials"))
}

/// The operator's file of the credential `name` (`KIND:NAME`):
/// `KIND/NAME` under [`dir`]. It may not exist.
pub fn file(name: &str) -> Result<PathBuf> {
    parse_name(name)?;
    let (kind, file) = name.split_once(':').unwrap_or_default();
    let dir = dir().ok_or_else(|| anyhow!("no credential directory (HOME is not set)"))?;
    Ok(dir.join(kind).join(file))
}

/// Load the operator's credential `name` (`KIND:NAME`, its file) as the
/// host applies it.
pub fn load(name: &str) -> Result<Credential> {
    let (kind, _) = parse_name(name)?;
    let path = file(name)?;
    let v = std::fs::read(&path).map(Secret::new).map_err(|e| {
        anyhow!(
            "no value for the credential {name}: {} cannot be read ({e})",
            path.display()
        )
    })?;
    let mut c = Credential::named(name);
    c.apply(kind, &v, path.parent())?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_have_a_kind() {
        assert_eq!(
            parse_name("kubeconfig:prod").unwrap(),
            (Kind::Kubeconfig, "prod")
        );
        assert!(parse_name("prod").is_err());
        assert!(parse_name("ftp:x").is_err());
        assert!(parse_name("bearer:../x").is_err());
        assert_eq!(
            parse_name("ssh:k3s-admin").unwrap(),
            (Kind::Ssh, "k3s-admin")
        );
    }

    /// A program's value is held for one provider, applied by its kind,
    /// and never printed; it is gone once the holding is.
    #[test]
    fn a_held_kubeconfig_is_its_providers_and_applied_by_its_parts() {
        let kc = "apiVersion: v1\ncurrent-context: c\ncontexts:\n- name: c\n  context: {cluster: k, user: u, namespace: shop}\n\
                  clusters:\n- name: k\n  cluster: {server: \"https://k.example:6443\", certificate-authority-data: Q0E=}\n\
                  users:\n- name: u\n  user: {token: s3cret}\n";
        let parts = BTreeMap::from([("kubeconfig".to_string(), Secret::new(kc.into()))]);
        let h = hold(Kind::Kubeconfig, "k8s", Given::Settings(parts));
        assert!(h.name().starts_with("kubeconfig:k8s."), "{}", h.name());
        assert!(held(h.name(), "other").is_none(), "another provider's");
        let c = held(h.name(), "k8s").unwrap().unwrap();
        assert_eq!(c.endpoint.as_deref(), Some("https://k.example:6443"));
        assert_eq!(c.namespace.as_deref(), Some("shop"));
        assert_eq!(c.ca.as_deref(), Some(&b"CA"[..]));
        assert_eq!(c.headers[0].1.expose(), b"Bearer s3cret");
        assert!(!format!("{c:?}").contains("s3cret"));
        let name = h.name().to_string();
        drop(h);
        assert!(held(&name, "k8s").is_none(), "released");
    }

    /// Settings of a kind that is one value are one setting.
    #[test]
    fn a_bearer_is_one_setting() {
        let one = BTreeMap::from([("token".to_string(), Secret::new(b"t".to_vec()))]);
        let h = hold(Kind::Bearer, "p", Given::Settings(one));
        let c = held(h.name(), "p").unwrap().unwrap();
        assert_eq!(c.headers[0].1.expose(), b"Bearer t");
        let two = BTreeMap::from([
            ("a".to_string(), Secret::new(b"t".to_vec())),
            ("b".to_string(), Secret::new(b"u".to_vec())),
        ]);
        let h = hold(Kind::Bearer, "p", Given::Settings(two));
        let e = held(h.name(), "p").unwrap().unwrap_err().to_string();
        assert_eq!(e, "a bearer credential is one setting, not a, b");
    }
}
