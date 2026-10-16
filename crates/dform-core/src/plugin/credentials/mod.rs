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
//! | `kubeconfig` | a kubeconfig (its current context), or its parts ([`kubeconfig`]) | its user's token, client certificate or exec plugin's answer (until it expires), its cluster's CA, server name and proxy; its server is the credential's endpoint, its namespace the credential's |
//! | `signed` | what the provider's signer makes of the operator's grant (`crate::plugin::signer`): the environment's variables or a file it names, the operator's file | as its material's recipe says: a bearer token, headers, a client certificate, a proxy, or each request signed by the signer as the host sends it |
//! | `ssh`        | an unencrypted OpenSSH private key | by the `ssh` provider (`use ssh { key = "NAME" }`), never to an HTTP call; only the operator's file ([`file`]) |
//!
//! Where a value comes from:
//!
//! - held for one provider ([`hold`]): a setting its handshake declares a
//!   credential (`kubeconfig`), as the program gives it (`use k8s {
//!   kubeconfig = cluster.kubeconfig }`, a secret revealed into the
//!   Configure, R-45). The engine holds it at that Configure under a name
//!   of its own, `KIND:PROVIDER.N`, and the provider is configured with
//!   the name; only that provider opens it, and it is gone once its
//!   session is configured again or its link stops;
//! - else the operator's, as dform.toml grants it (`[providers.k8s]
//!   credentials = ["kubeconfig:prod"]`): the file
//!   `$XDG_CONFIG_HOME/dform/credentials/KIND/NAME` (or under
//!   `DFORM_CREDENTIALS`), or, named `KIND:environment` ([`ENVIRONMENT`]),
//!   the environment's of a kind that has one (a kubeconfig's:
//!   `KUBECONFIG`, `~/.kube/config`, a pod's service account; a signed
//!   one's: the variables its provider's signer names, else the file it
//!   names). A provider is told, in its
//!   Configure's `credentials`, the one dform.toml grants it of each kind,
//!   and a provider the program does not configure the environment's of
//!   each kind it declares a setting of; the host opens it only with the
//!   grant: nothing in the environment reaches a provider unless the
//!   program or dform.toml gives it.

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
    Signed,
    Ssh,
}

pub const KINDS: [&str; 7] = [
    "bearer",
    "basic",
    "header",
    "tls",
    "kubeconfig",
    "signed",
    "ssh",
];

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
            "signed" => Kind::Signed,
            "ssh" => Kind::Ssh,
            _ => return None,
        })
    }

    /// Whether the environment has one of the kind (a kubeconfig's:
    /// `KUBECONFIG`; a signed one's: what its signer names), which
    /// `KIND:environment` names ([`ENVIRONMENT`]).
    pub fn in_environment(self) -> bool {
        matches!(self, Kind::Kubeconfig | Kind::Signed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Bearer => "bearer",
            Kind::Basic => "basic",
            Kind::Header => "header",
            Kind::Tls => "tls",
            Kind::Kubeconfig => "kubeconfig",
            Kind::Signed => "signed",
            Kind::Ssh => "ssh",
        }
    }
}

/// The NAME of the environment's credential of a kind that has one
/// ([`Kind::in_environment`]): `kubeconfig:environment` is the
/// environment's kubeconfig, never an operator's file.
pub const ENVIRONMENT: &str = "environment";

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
    /// The name TLS offers and verifies in place of the URL's host (a
    /// kubeconfig's `tls-server-name`).
    pub server_name: Option<String>,
    /// The proxy its requests go through, in place of the environment's
    /// (a kubeconfig's `proxy-url`): an `http://` URL.
    pub proxy: Option<String>,
    /// When what it applies stops working (an exec plugin's token, its
    /// `expirationTimestamp`): the host opens it again then.
    pub expires: Option<std::time::SystemTime>,
    /// The material each request is signed with by the provider's signer
    /// as the host sends it (`signed`, its recipe `Recipe::Signed`).
    pub signed: Option<crate::plugin::signer::Material>,
}

impl Credential {
    /// Whether it has expired by `at`.
    pub fn expired_by(&self, at: std::time::SystemTime) -> bool {
        self.expires.is_some_and(|e| e <= at)
    }

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
            Kind::Signed => bail!(
                "the credential {name} is signed: its provider's signer makes it, which the \
                 host runs"
            ),
            Kind::Ssh => bail!(
                "the credential {name} is an SSH key: the ssh provider uses it (`use ssh {{ key \
                 = .. }}`), never an HTTP call"
            ),
        }
        Ok(())
    }
}

/// The credential `name` of `kind` a provider's settings give, by
/// setting, as the host applies it: one is the value; several are a
/// kind's parts (a kubeconfig's `host`, `ca`, `token`, ..,
/// [`kubeconfig::Kubeconfig::of_parts`]).
fn of_settings(kind: Kind, name: &str, parts: &BTreeMap<String, Secret>) -> Result<Credential> {
    let mut c = Credential::named(name);
    if kind == Kind::Kubeconfig {
        kubeconfig::Kubeconfig::of_parts(name, parts)?.apply(&mut c)?;
        return Ok(c);
    }
    let mut values = parts.values();
    let (Some(v), None) = (values.next(), values.next()) else {
        bail!(
            "a {} credential is one setting, not {}",
            kind.as_str(),
            parts.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };
    c.apply(kind, v, None)?;
    Ok(c)
}

/// A credential held for a provider: its kind, the provider, the
/// settings that give it.
struct Held {
    kind: Kind,
    to: String,
    parts: BTreeMap<String, Secret>,
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

/// Hold the settings `parts` as a credential of `kind` for the provider
/// `to` (its name in dform.toml, as its host's grants say): in this
/// process's memory, under a name of its own, `KIND:TO.N`, which only `to`
/// opens ([`held`]), until the holding is dropped.
pub fn hold(kind: Kind, to: &str, parts: BTreeMap<String, Secret>) -> Holding {
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
            parts,
        },
    );
    Holding(name)
}

/// The credential `name` as the host applies it, if it is held for the
/// provider `to` ([`hold`]); `None` when it is not one held for it. Its
/// error says why it does not load, never a value.
pub fn held(name: &str, to: &str) -> Option<Result<Credential>> {
    // Loaded with the map let go: a kubeconfig's exec plugin runs.
    let (kind, parts) = {
        let map = held_map();
        let h = map.get(name).filter(|h| h.to == to)?;
        (h.kind, h.parts.clone())
    };
    Some(of_settings(kind, name, &parts))
}

/// The credential `name` held for the provider `to` as one setting, its
/// value as given (a `signed` credential's, which stands for the file its
/// provider's signer names); `None` when it is not one held for it.
pub fn held_value(name: &str, to: &str) -> Option<Result<Secret>> {
    let map = held_map();
    let h = map.get(name).filter(|h| h.to == to)?;
    let mut values = h.parts.values();
    Some(match (values.next(), values.next()) {
        (Some(v), None) => Ok(v.clone()),
        _ => Err(anyhow!(
            "a {} credential is one setting, not {}",
            h.kind.as_str(),
            h.parts.keys().cloned().collect::<Vec<_>>().join(", ")
        )),
    })
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

/// Load the operator's credential `name` as the host applies it: its
/// file, or, `KIND:environment`, the environment's ([`ENVIRONMENT`]).
pub fn load(name: &str) -> Result<Credential> {
    let (kind, rest) = parse_name(name)?;
    if rest == ENVIRONMENT {
        return environment(kind, name);
    }
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

/// The environment's credential of `kind`, named `name`
/// (`KIND:environment`), read now.
fn environment(kind: Kind, name: &str) -> Result<Credential> {
    let mut c = Credential::named(name);
    match kind {
        Kind::Kubeconfig => kubeconfig::Kubeconfig::environment()?.apply(&mut c)?,
        _ => bail!(
            "the credential {name}: a {} credential has no source in the environment",
            kind.as_str()
        ),
    }
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
        let h = hold(Kind::Kubeconfig, "k8s", parts);
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
        let h = hold(Kind::Bearer, "p", one);
        let c = held(h.name(), "p").unwrap().unwrap();
        assert_eq!(c.headers[0].1.expose(), b"Bearer t");
        let two = BTreeMap::from([
            ("a".to_string(), Secret::new(b"t".to_vec())),
            ("b".to_string(), Secret::new(b"u".to_vec())),
        ]);
        let h = hold(Kind::Bearer, "p", two);
        let e = held(h.name(), "p").unwrap().unwrap_err().to_string();
        assert_eq!(e, "a bearer credential is one setting, not a, b");
    }
}
