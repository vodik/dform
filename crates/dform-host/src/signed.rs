//! A credential its provider makes (`signed`, `kubeconfig`,
//! `dform_core::plugin::credentials`): its provider's signer
//! (`plugin::signer`, sans I/O) makes its material of the operator's
//! grant, the host performing each thing it needs, and the host applies
//! the material as its recipe says, asking the signer to sign each request
//! when the recipe says `Signed`. The host knows nothing of what a signer
//! signs for: it performs, holds and applies.
//!
//! What a need gets is the grant's: the value it gives (`given`: the
//! settings a program gave the provider, by setting, or the operator's
//! file `credentials/KIND/NAME`, named by its kind; none for
//! `KIND:environment`); the environment's variables the signer names, for
//! `KIND:environment` only; the files it names (`~/` the operator's home,
//! a relative path the operator's file's directory's, else the working
//! directory's). A fetch is that request, to exactly its URL,
//! unauthenticated, through the host's HTTP (paced and traced as every
//! request is). An exec is not performed yet.

use crate::services::Services;
use dform_core::plugin::credentials::{self, Credential, ENVIRONMENT, Secret};
use dform_core::plugin::host::{Calls, Error, HttpRequest};
use dform_core::plugin::signer::{Got, Material, Need, Outcome, Recipe, Request, Response, Signer};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many rounds of needs a signer may ask before its material: a
/// signer that keeps asking is refused.
const ROUNDS: usize = 8;

/// The seconds since the epoch, now.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What the grant gives a signer's needs.
pub enum Source {
    /// `KIND:environment`: the variables and the files it names.
    Environment,
    /// The operator's file `credentials/KIND/NAME`: its value, named by its
    /// kind, and its directory, which a relative path is under.
    File {
        kind: &'static str,
        value: Secret,
        dir: PathBuf,
    },
    /// The settings a program gave the provider (held for it), by setting.
    Settings(BTreeMap<String, Secret>),
}

impl Source {
    /// The source of the credential `name` for the provider `provider`:
    /// the settings held for it, else, granted, the environment or the
    /// operator's file.
    pub fn of(
        name: &str,
        provider: &str,
        granted: impl Fn() -> Result<(), Error>,
    ) -> Result<Source, Error> {
        let fatal = |e: anyhow::Error| {
            Error::fatal(format!("provider {provider}: the credential {name}: {e:#}"))
        };
        if let Some(settings) = credentials::held_settings(name, provider) {
            return Ok(Source::Settings(settings));
        }
        granted()?;
        let (kind, rest) = credentials::parse_name(name).map_err(fatal)?;
        if rest == ENVIRONMENT {
            return Ok(Source::Environment);
        }
        let path = credentials::file(name).map_err(fatal)?;
        let value = std::fs::read(&path).map(Secret::new).map_err(|e| {
            fatal(anyhow::anyhow!(
                "no value: {} cannot be read ({e})",
                path.display()
            ))
        })?;
        Ok(Source::File {
            kind: kind.as_str(),
            value,
            dir: path.parent().map(PathBuf::from).unwrap_or_default(),
        })
    }

    /// The value it gives: the settings, or the operator's file.
    fn given(&self) -> Vec<(String, Vec<u8>)> {
        match self {
            Source::Environment => Vec::new(),
            Source::File { kind, value, .. } => vec![(kind.to_string(), value.expose().to_vec())],
            Source::Settings(settings) => settings
                .iter()
                .map(|(k, v)| (k.clone(), v.expose().to_vec()))
                .collect(),
        }
    }

    /// The environment's variables of `vars` that are set, for
    /// `KIND:environment`; none for a grant that gives a value.
    fn env(&self, vars: &[String]) -> Vec<(String, String)> {
        match self {
            Source::Environment => vars
                .iter()
                .filter_map(|v| Some((v.clone(), std::env::var(v).ok()?)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The file `path` names: `~/` the operator's home; relative, the
    /// operator's file's directory's, else the working directory's.
    fn path(&self, path: &str) -> Option<PathBuf> {
        if let Some(rest) = path.strip_prefix("~/") {
            return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
        }
        Some(match self {
            Source::File { dir, .. } => dir.join(path),
            _ => PathBuf::from(path),
        })
    }
}

/// A signer's run to its material: the needs it asks, each performed by
/// `host` from `source`, what each got handed back, until it answers the
/// material.
pub struct Steps<'a> {
    pub host: &'a Services,
    pub signer: &'a (dyn Signer + Send + Sync),
    pub source: Source,
    /// The credential's name, for messages.
    pub name: &'a str,
}

impl Steps<'_> {
    /// The material, from the signer's first needs.
    pub fn material(self) -> Result<Material, Error> {
        let first = self.signer.needs().map_err(|e| self.refused(e))?;
        self.run(first)
    }

    /// The material, renewed from `material` before it expires.
    pub fn renewed(self, material: &Material) -> Result<Material, Error> {
        match self
            .signer
            .refresh(material, now())
            .map_err(|e| self.refused(e))?
        {
            Outcome::Material(m) => Ok(m),
            Outcome::More(needs) => self.run(needs),
        }
    }

    /// Each round of `needs` performed, and the signer stepped with every
    /// got so far, until its material.
    fn run(&self, mut needs: Vec<Need>) -> Result<Material, Error> {
        let mut got = Vec::new();
        for _ in 0..ROUNDS {
            for need in &needs {
                got.push(self.perform(need)?);
            }
            match self.signer.step(&got).map_err(|e| self.refused(e))? {
                Outcome::Material(m) => return Ok(m),
                Outcome::More(more) => needs = more,
            }
        }
        Err(self.refused(format!("it asked more than {ROUNDS} times")))
    }

    fn refused(&self, why: String) -> Error {
        Error::fatal(format!(
            "provider {}: the credential {}: {why}",
            self.host.grants().provider,
            self.name
        ))
    }

    /// What `need` gets from the grant.
    fn perform(&self, need: &Need) -> Result<Got, Error> {
        Ok(match need {
            Need::Given => Got::Given(self.source.given()),
            Need::Env(vars) => Got::Env(self.source.env(vars)),
            Need::File(path) => {
                Got::File(self.source.path(path).and_then(|p| std::fs::read(p).ok()))
            }
            Need::Exec(c) => {
                return Err(self.refused(format!(
                    "its signer asks to run {:?}, which the host does not do yet (WORK.org: a \
                     signer's exec need)",
                    c.argv.first().map_or("", String::as_str)
                )));
            }
            Need::Fetch(req) => Got::Fetch(self.fetch(req)?),
        })
    }

    /// `req`, sent as it is to its URL, unauthenticated: its answer, and
    /// when it came.
    fn fetch(&self, req: &Request) -> Result<Response, Error> {
        let sent = HttpRequest {
            method: req.method.clone(),
            url: req.url.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            ..HttpRequest::default()
        };
        let r = self.host.send(sent, None, None)?;
        Ok(Response {
            status: r.status,
            headers: r.headers,
            body: r.body,
            at: now(),
        })
    }
}

/// The credential `name` applied as `material`'s recipe says.
pub fn applied(name: &str, material: Material) -> Credential {
    let mut c = Credential {
        name: name.to_string(),
        endpoint: material.endpoint.clone(),
        namespace: material.namespace.clone(),
        expires: material
            .expires
            .map(|e| UNIX_EPOCH + Duration::from_secs(e)),
        ..Credential::default()
    };
    for recipe in &material.recipe {
        match recipe {
            Recipe::Bearer(token) => c.headers.push((
                "Authorization".into(),
                Secret::new(format!("Bearer {token}").into_bytes()),
            )),
            Recipe::Headers(headers) => c.headers.extend(
                headers
                    .iter()
                    .map(|(k, v)| (k.clone(), Secret::new(v.as_bytes().to_vec()))),
            ),
            Recipe::Signed => {}
            Recipe::TlsClient(t) => {
                c.client_cert = Some((Secret::new(t.cert.clone()), Secret::new(t.key.clone())));
            }
            Recipe::Proxy(url) => c.proxy = Some(url.clone()),
            Recipe::Ca(pem) => c.ca = Some(pem.clone()),
            Recipe::Insecure => c.insecure = true,
            Recipe::ServerName(n) => c.server_name = Some(n.clone()),
        }
    }
    c.material = Some(material);
    c
}

/// `req` signed by `signer` with `material`, as it is to be sent.
pub fn sign(
    signer: &(dyn Signer + Send + Sync),
    req: HttpRequest,
    material: &Material,
    name: &str,
) -> Result<HttpRequest, Error> {
    let unsigned = Request {
        method: req.method.clone(),
        url: req.url.clone(),
        headers: req.headers.clone(),
        body: req.body.clone(),
    };
    let signed = signer
        .sign(unsigned, material, now())
        .map_err(|e| Error::fatal(format!("the credential {name}: signing {}: {e}", req.url)))?;
    Ok(HttpRequest {
        method: signed.method,
        url: signed.url,
        headers: signed.headers,
        body: signed.body,
        ..req
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator's file gives its value, named by its kind, and a
    /// relative path is under its directory; the environment gives no
    /// value, and a relative path is the working directory's; a program's
    /// settings give each by its name, and no variable.
    #[test]
    fn each_grant_gives_its_own() {
        let file = Source::File {
            kind: "kubeconfig",
            value: Secret::new(b"kc".to_vec()),
            dir: PathBuf::from("/creds/kubeconfig"),
        };
        assert_eq!(file.given(), [("kubeconfig".to_string(), b"kc".to_vec())]);
        assert_eq!(file.path("ca.pem"), Some("/creds/kubeconfig/ca.pem".into()));
        assert_eq!(file.path("/etc/ca.pem"), Some("/etc/ca.pem".into()));
        assert!(Source::Environment.given().is_empty());
        assert_eq!(Source::Environment.path("ca.pem"), Some("ca.pem".into()));
        let settings = Source::Settings(BTreeMap::from([
            ("host".to_string(), Secret::new(b"k".to_vec())),
            ("token".to_string(), Secret::new(b"t".to_vec())),
        ]));
        assert_eq!(
            settings.given(),
            [
                ("host".to_string(), b"k".to_vec()),
                ("token".to_string(), b"t".to_vec())
            ]
        );
        assert!(settings.env(&["PATH".to_string()]).is_empty());
    }
}
