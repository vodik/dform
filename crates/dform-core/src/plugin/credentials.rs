//! Credentials by name (R-13b): a provider opens a credential the operator
//! granted it (`[providers.k8s] credentials = ["kubeconfig:prod"]`) through
//! the host (`dform:host/secrets`), and the host applies the value to its
//! own call (`dform:host/http`'s `auth`). The value is never serialized to
//! a provider.
//!
//! A name is `KIND:NAME`; the kind says how the host applies it:
//!
//! | kind         | the value                         | applied as                              |
//! |--------------|-----------------------------------|-----------------------------------------|
//! | `bearer`     | a token                           | `Authorization: Bearer TOKEN`           |
//! | `basic`      | `USER:PASSWORD`                   | `Authorization: Basic ..`               |
//! | `header`     | `NAME: VALUE`                     | that header                             |
//! | `tls`        | PEM: a certificate chain and key  | the TLS session's client certificate    |
//! | `kubeconfig` | a kubeconfig (its current context)| its user's token or client certificate, its cluster's CA; its server is the credential's endpoint |
//!
//! Where a value comes from: the program (`provider k8s { kubeconfig =
//! cluster.kubeconfig }` registers the secret under the name the host
//! applies, [`provide`]; R-45's reveal), else the operator's file
//! `$XDG_CONFIG_HOME/dform/credentials/KIND/NAME` (or under
//! `DFORM_CREDENTIALS`).

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bearer,
    Basic,
    Header,
    Tls,
    Kubeconfig,
}

pub const KINDS: [&str; 5] = ["bearer", "basic", "header", "tls", "kubeconfig"];

/// `KIND:NAME`, checked.
pub fn parse_name(name: &str) -> Result<(Kind, &str)> {
    let Some((kind, rest)) = name.split_once(':') else {
        bail!(
            "the credential {name:?} names no kind: write KIND:NAME, KIND one of {}",
            KINDS.join(", ")
        );
    };
    let kind = match kind {
        "bearer" => Kind::Bearer,
        "basic" => Kind::Basic,
        "header" => Kind::Header,
        "tls" => Kind::Tls,
        "kubeconfig" => Kind::Kubeconfig,
        _ => bail!(
            "the credential {name:?} has the kind {kind:?}: expected one of {}",
            KINDS.join(", ")
        ),
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

/// Values the program gave, by credential name ([`provide`]).
static PROVIDED: Mutex<BTreeMap<String, Secret>> = Mutex::new(BTreeMap::new());

/// Register `value` as the credential `name`: what a program's provider
/// block gives (`provider k8s { kubeconfig = .. }`) once its secret is
/// revealed into the run (R-45). It lives in this process's memory only.
///
/// TODO(R-45): `plugin::providers`' Configure calls this with the revealed
/// setting under the name `KIND:BLOCK` the provider's grant lists.
pub fn provide(name: &str, value: Secret) {
    PROVIDED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), value);
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

/// The value of the credential `name`: the program's, else the operator's
/// file.
fn value(name: &str, kind: &str, file: &str) -> Result<Secret> {
    if let Some(v) = PROVIDED.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
        return Ok(v.clone());
    }
    let dir = dir().ok_or_else(|| anyhow!("no credential directory (HOME is not set)"))?;
    let path = dir.join(kind).join(file);
    let bytes = std::fs::read(&path).map_err(|e| {
        anyhow!(
            "no value for the credential {name}: the program gives none and {} cannot be read \
             ({e})",
            path.display()
        )
    })?;
    Ok(Secret::new(bytes))
}

/// Load the credential `name` (`KIND:NAME`) as the host applies it.
pub fn load(name: &str) -> Result<Credential> {
    let (kind, file) = parse_name(name)?;
    let kind_name = name.split_once(':').map_or("", |(k, _)| k);
    let v = value(name, kind_name, file)?;
    let mut c = Credential {
        name: name.to_string(),
        ..Credential::default()
    };
    match kind {
        Kind::Bearer => {
            let token = v.text(name)?.trim();
            c.headers.push((
                "Authorization".into(),
                Secret::new(format!("Bearer {token}").into_bytes()),
            ));
        }
        Kind::Basic => {
            use base64::Engine;
            let pair = v.text(name)?.trim();
            if !pair.contains(':') {
                bail!("the credential {name} is not USER:PASSWORD");
            }
            let enc = base64::engine::general_purpose::STANDARD.encode(pair);
            c.headers.push((
                "Authorization".into(),
                Secret::new(format!("Basic {enc}").into_bytes()),
            ));
        }
        Kind::Header => {
            let line = v.text(name)?.trim();
            let (h, val) = line
                .split_once(':')
                .ok_or_else(|| anyhow!("the credential {name} is not `NAME: VALUE`"))?;
            c.headers.push((
                h.trim().to_string(),
                Secret::new(val.trim().as_bytes().to_vec()),
            ));
        }
        Kind::Tls => {
            let pem = v.text(name)?;
            if !pem.contains("PRIVATE KEY") || !pem.contains("CERTIFICATE") {
                bail!("the credential {name} is not a PEM certificate chain and key");
            }
            c.client_cert = Some((v.clone(), v.clone()));
        }
        Kind::Kubeconfig => kubeconfig(name, v.text(name)?, &mut c)?,
    }
    Ok(c)
}

/// A kubeconfig's current context: its cluster's server and CA, its
/// user's token or client certificate.
fn kubeconfig(name: &str, text: &str, c: &mut Credential) -> Result<()> {
    use base64::Engine;
    let doc: serde_yaml::Value =
        serde_yaml::from_str(text).with_context(|| format!("the credential {name}"))?;
    let s = |v: &serde_yaml::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    let named = |list: &str, want: &str| -> Option<serde_yaml::Value> {
        doc.get(list)?
            .as_sequence()?
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(want))
            .cloned()
    };
    let current = s(&doc, "current-context")
        .ok_or_else(|| anyhow!("the credential {name}: the kubeconfig has no current-context"))?;
    let ctx = named("contexts", &current)
        .and_then(|c| c.get("context").cloned())
        .ok_or_else(|| anyhow!("the credential {name}: no context {current:?}"))?;
    let cluster = s(&ctx, "cluster")
        .and_then(|n| named("clusters", &n))
        .and_then(|c| c.get("cluster").cloned())
        .ok_or_else(|| anyhow!("the credential {name}: the context names no cluster"))?;
    let user = s(&ctx, "user")
        .and_then(|n| named("users", &n))
        .and_then(|u| u.get("user").cloned())
        .unwrap_or(serde_yaml::Value::Null);
    let b64 = |v: &serde_yaml::Value, k: &str| -> Result<Option<Vec<u8>>> {
        match s(v, k) {
            None => Ok(None),
            Some(d) => base64::engine::general_purpose::STANDARD
                .decode(d.trim())
                .map(Some)
                .with_context(|| format!("the credential {name}: {k}")),
        }
    };
    c.endpoint = s(&cluster, "server");
    c.ca = b64(&cluster, "certificate-authority-data")?;
    c.insecure = cluster
        .get("insecure-skip-tls-verify")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    if let Some(token) = s(&user, "token") {
        c.headers.push((
            "Authorization".into(),
            Secret::new(format!("Bearer {token}").into_bytes()),
        ));
    }
    if let (Some(cert), Some(key)) = (
        b64(&user, "client-certificate-data")?,
        b64(&user, "client-key-data")?,
    ) {
        c.client_cert = Some((Secret::new(cert), Secret::new(key)));
    }
    if c.headers.is_empty() && c.client_cert.is_none() {
        bail!(
            "the credential {name}: its user has neither a token nor client-certificate-data \
             (an exec plugin is not run by the host)"
        );
    }
    Ok(())
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
    }

    /// A program's value is applied by its kind; it never prints.
    #[test]
    fn a_provided_kubeconfig_is_applied_by_its_parts() {
        let kc = "apiVersion: v1\ncurrent-context: c\ncontexts:\n- name: c\n  context: {cluster: k, user: u}\n\
                  clusters:\n- name: k\n  cluster: {server: \"https://k.example:6443\", certificate-authority-data: Q0E=}\n\
                  users:\n- name: u\n  user: {token: s3cret}\n";
        provide(
            "kubeconfig:test-provided",
            Secret::new(kc.as_bytes().to_vec()),
        );
        let c = load("kubeconfig:test-provided").unwrap();
        assert_eq!(c.endpoint.as_deref(), Some("https://k.example:6443"));
        assert_eq!(c.ca.as_deref(), Some(&b"CA"[..]));
        assert_eq!(c.headers[0].1.expose(), b"Bearer s3cret");
        assert!(!format!("{c:?}").contains("s3cret"));
    }
}
