//! A `signed` credential (`dform_core::plugin::credentials`): its
//! provider's signer (`plugin::signer`, sans I/O) makes its material of
//! the operator's grant, the host performing each thing it needs, and
//! the host applies the material as its recipe says, asking the signer to
//! sign each request when the recipe is `Signed`. The host knows nothing
//! of what a signer signs for: it performs, holds and applies.
//!
//! What a need gets is the grant's: for `signed:environment`, the
//! environment's variables the signer names, the file it names (`~/` the
//! operator's home); for `signed:NAME`, no variable, and the operator's
//! file `credentials/signed/NAME` (or a setting a program gave, held for
//! the provider) in place of any file. A fetch is that request, to
//! exactly its URL, unauthenticated, through the host's HTTP (paced and
//! traced as every request is). An exec is not performed yet.

use crate::services::Services;
use dform_core::plugin::credentials::{self, Credential, ENVIRONMENT, Secret};
use dform_core::plugin::host::{Calls, Error, HttpRequest};
use dform_core::plugin::signer::{Got, Material, Need, Outcome, Recipe, Request, Response, Signer};
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
    /// `signed:environment`: the variables and the files it names.
    Environment,
    /// The operator's file, or a program's setting held for the provider,
    /// in place of any file; no variable.
    Given(Secret),
}

impl Source {
    /// The source of the credential `name` for the provider `provider`:
    /// a setting held for it, else, granted, the environment or the
    /// operator's file.
    pub fn of(
        name: &str,
        provider: &str,
        granted: impl Fn() -> Result<(), Error>,
    ) -> Result<Source, Error> {
        let fatal = |e: anyhow::Error| {
            Error::fatal(format!("provider {provider}: the credential {name}: {e:#}"))
        };
        if let Some(held) = credentials::held_value(name, provider) {
            return held.map(Source::Given).map_err(fatal);
        }
        granted()?;
        let (_, rest) = credentials::parse_name(name).map_err(fatal)?;
        if rest == ENVIRONMENT {
            return Ok(Source::Environment);
        }
        let path = credentials::file(name).map_err(fatal)?;
        std::fs::read(&path)
            .map(|b| Source::Given(Secret::new(b)))
            .map_err(|e| {
                fatal(anyhow::anyhow!(
                    "no value: {} cannot be read ({e})",
                    path.display()
                ))
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
        Ok(match (need, &self.source) {
            (Need::Env(vars), Source::Environment) => Got::Env(
                vars.iter()
                    .filter_map(|v| Some((v.clone(), std::env::var(v).ok()?)))
                    .collect(),
            ),
            (Need::Env(_), Source::Given(_)) => Got::Env(Vec::new()),
            (Need::File(path), Source::Environment) => {
                Got::File(home(path).and_then(|p| std::fs::read(p).ok()))
            }
            (Need::File(_), Source::Given(value)) => Got::File(Some(value.expose().to_vec())),
            (Need::Exec(argv), _) => {
                return Err(self.refused(format!(
                    "its signer asks to run {:?}, which the host does not do yet (WORK.org: a \
                     signer's exec need)",
                    argv.first().map_or("", String::as_str)
                )));
            }
            (Need::Fetch(req), _) => Got::Fetch(self.fetch(req)?),
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

/// `path`, its `~/` the operator's home.
fn home(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// The credential `name` applied as `material`'s recipe says.
pub fn applied(name: &str, material: Material) -> Credential {
    let mut c = Credential {
        name: name.to_string(),
        expires: material
            .expires
            .map(|e| UNIX_EPOCH + Duration::from_secs(e)),
        ..Credential::default()
    };
    match &material.recipe {
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
            c.ca = t.ca.clone();
        }
        Recipe::Proxy(url) => c.proxy = Some(url.clone()),
    }
    c.signed = Some(material);
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
