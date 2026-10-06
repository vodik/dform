//! What dform offers a provider beyond the protocol (R-13b, R-25): the
//! credentialed transports of `wit/host/dform-host.wit` (`dform:host`),
//! as plain Rust, and the grants that say which a provider may use.
//!
//! The interfaces are defined once in WIT and offered on two transports:
//! imports of a component the wasm host runs (`dform-host`'s `wasm`
//! feature), and the `Host` gRPC service a native provider calls back into
//! (`proto/dform/v1/host.proto`). Both serve [`Calls`]; the SDK
//! (`dform-sdk`) is [`Calls`] on the provider's side, over whichever
//! transport it is built for, so a provider calls `host.http.send(..)` the
//! same way on both.
//!
//! Grants, not walls: a provider's manifest is what it imports (a
//! component's own import list; what a native SDK provider declares), and
//! dform.toml's `[providers.NAME] allow` grants what is beyond the host's
//! own interfaces (`wasi:sockets`, `wasi:http`, `wasi:filesystem`), and
//! `credentials` the credentials it may open by name ([`super::credentials`]).
//! `dform provider check` prints both. A provider using `wasi:sockets`
//! directly is allowed when granted, and gives up the host's TLS, retries,
//! waits and audit for what it does there, in the open.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// How a failed host call is handled: R-81's classes, as
/// [`super::policy::Class`] reads a provider call's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// It fails the same way again.
    Final,
    /// Nothing changed, and the failure is transient (a 429, a 5xx, a
    /// refused connection).
    Retryable,
    /// No answer came: it may have taken effect (a timeout after the
    /// request was sent).
    MaybeApplied,
}

/// A failed host call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub class: Class,
    pub message: String,
}

impl Error {
    pub fn fatal(message: impl Into<String>) -> Error {
        Error {
            class: Class::Final,
            message: message.into(),
        }
    }

    pub fn retryable(message: impl Into<String>) -> Error {
        Error {
            class: Class::Retryable,
            message: message.into(),
        }
    }

    pub fn maybe_applied(message: impl Into<String>) -> Error {
        Error {
            class: Class::MaybeApplied,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.class {
            // The prefix `policy::retryable` reads, so a provider that
            // passes the message on in a refusal is retried by dform.
            Class::Retryable => write!(f, "retryable: {}", self.message),
            Class::Final | Class::MaybeApplied => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

/// A read of what the world has not reached yet is not an error: the
/// engine waits on it (R-81's `--wait`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    NotYet(String),
    Error(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Failure {
        Failure::Error(e)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NotYet(m) => write!(f, "not yet: {m}"),
            Failure::Error(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Failure {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// At most this long, else the host's default.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An SSH host to reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub user: String,
    /// 22 when absent.
    pub port: Option<u16>,
}

/// A command's outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Run {
    /// -1 when the command was killed by a signal.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFile {
    pub path: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

/// A credential or a tunnel, as a transport names it: a component's
/// resource, the gRPC service's number. Never the value.
pub type Handle = u64;

/// A credential opened by name ([`Calls::open`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub handle: Handle,
    /// The endpoint it is for, when it names one (a kubeconfig's server).
    pub endpoint: Option<String>,
}

/// The host's interfaces as calls: `dform:host`'s `secrets`, `http`,
/// `ssh`, `git` and `log`. dform serves them (`dform-host`); a provider
/// built with the SDK calls them over its transport.
pub trait Calls: Send {
    /// `secrets.open`: the credential `name` (`KIND:NAME`), refused unless
    /// granted.
    fn open(&mut self, name: &str) -> Result<Opened, Error>;
    /// `http.send`, with the credential `auth` applied and through the
    /// tunnel `via`.
    fn send(
        &mut self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, Error>;
    /// `ssh.exec`.
    fn exec(&mut self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure>;
    /// `ssh.read`.
    fn read(&mut self, on: &Target, path: &str) -> Result<Vec<u8>, Failure>;
    /// `ssh.write`.
    fn write(&mut self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error>;
    /// `ssh.forward`: a tunnel.
    fn forward(&mut self, via: &Target, to: &Endpoint) -> Result<Handle, Error>;
    /// Where the tunnel `h` leads.
    fn tunnel(&self, h: Handle) -> Option<Endpoint>;
    /// `git.read`.
    fn git_read(&mut self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, Error>;
    /// `git.commit`: the new commit's id.
    fn git_commit(
        &mut self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, Error>;
    /// `log.log`.
    fn log(&mut self, level: Level, message: &str);
}

/// The host's own interfaces, by the names a manifest and a grant use.
pub const HOST_INTERFACES: [&str; 6] = [
    "dform:host/types",
    "dform:host/secrets",
    "dform:host/http",
    "dform:host/ssh",
    "dform:host/git",
    "dform:host/log",
];

/// The WASI packages a provider uses only when granted: each reaches past
/// the host's mediation (its TLS, retries, waits and audit).
pub const GRANTABLE: [&str; 3] = ["wasi:filesystem", "wasi:http", "wasi:sockets"];

/// What a provider uses: the interfaces it imports, versions dropped
/// (`dform:host/http`, `wasi:sockets/tcp`). A component's is its own
/// import list; a native provider's is what it declares (the SDK's).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub imports: BTreeSet<String>,
    /// Whether the provider is a component, whose imports are all it can
    /// reach; a native process runs as the user, and its manifest is a
    /// declaration.
    pub sandboxed: bool,
}

/// `wasi:sockets/tcp@0.2.12` -> `wasi:sockets/tcp`.
pub fn unversioned(name: &str) -> &str {
    name.split_once('@').map_or(name, |(n, _)| n)
}

/// `wasi:sockets/tcp` -> `wasi:sockets`.
pub fn package(interface: &str) -> &str {
    interface.split_once('/').map_or(interface, |(p, _)| p)
}

impl Manifest {
    /// A manifest of `imports` (versions dropped).
    pub fn of<'a>(imports: impl IntoIterator<Item = &'a str>, sandboxed: bool) -> Manifest {
        Manifest {
            imports: imports
                .into_iter()
                .map(|i| unversioned(i).to_string())
                .collect(),
            sandboxed,
        }
    }

    /// The host interfaces it uses, short (`http`, `ssh`).
    pub fn host_interfaces(&self) -> Vec<&str> {
        self.imports
            .iter()
            .filter(|i| HOST_INTERFACES.contains(&i.as_str()) && i.as_str() != "dform:host/types")
            .filter_map(|i| i.strip_prefix("dform:host/"))
            .collect()
    }

    /// The grantable packages it uses (`wasi:sockets`).
    pub fn needs(&self) -> BTreeSet<&str> {
        self.imports
            .iter()
            .map(|i| package(i))
            .filter(|p| GRANTABLE.contains(p))
            .collect()
    }
}

/// What dform.toml grants a provider: `[providers.NAME] allow` and
/// `credentials`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// The provider's name in dform.toml, for messages.
    pub provider: String,
    /// Grantable packages beyond the host's interfaces (`wasi:sockets`).
    pub allow: BTreeSet<String>,
    /// Credentials it may open, by name (`kubeconfig:prod`).
    pub credentials: BTreeSet<String>,
}

impl Grants {
    /// No grant, for the provider `name`.
    pub fn none(name: &str) -> Grants {
        Grants {
            provider: name.to_string(),
            ..Grants::default()
        }
    }

    /// Refuse a manifest that uses a grantable package this does not
    /// grant, naming the provider and the package.
    pub fn admit(&self, manifest: &Manifest) -> Result<(), String> {
        let missing: Vec<&str> = manifest
            .needs()
            .into_iter()
            .filter(|p| !self.allow.contains(*p))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let list = missing
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "provider {} imports {} and is not granted {}: add `allow = [{list}]` to \
             [providers.{}] in dform.toml (it then reaches past the host's TLS, retries, waits \
             and audit)",
            self.provider,
            missing.join(", "),
            if missing.len() == 1 { "it" } else { "them" },
            self.provider
        ))
    }

    /// Refuse to open a credential this does not grant, naming the
    /// provider and the credential.
    pub fn credential(&self, name: &str) -> Result<(), Error> {
        if self.credentials.contains(name) {
            return Ok(());
        }
        Err(Error::fatal(format!(
            "provider {} is not granted the credential {name}: add it to [providers.{}] \
             credentials in dform.toml",
            self.provider, self.provider
        )))
    }

    /// The report's lines: what the manifest imports and what is granted.
    pub fn describe(&self, manifest: Option<&Manifest>) -> Vec<String> {
        let list = |v: Vec<&str>| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.join(", ")
            }
        };
        let mut out = Vec::new();
        match manifest {
            Some(m) => {
                out.push(format!("imports: host {}", list(m.host_interfaces())));
                let wasi: Vec<&str> = m.needs().into_iter().collect();
                out.push(format!("imports: beyond the host {}", list(wasi)));
                if !m.sandboxed {
                    out.push(
                        "native: a process runs as the user; its grants are a declaration, not \
                         a sandbox"
                            .to_string(),
                    );
                }
            }
            None => out.push("imports: undeclared (not an SDK provider)".to_string()),
        }
        out.push(format!(
            "granted: {}",
            list(self.allow.iter().map(String::as_str).collect())
        ));
        out.push(format!(
            "credentials: {}",
            list(self.credentials.iter().map(String::as_str).collect())
        ));
        out
    }
}

/// The grants of the run's providers, by source path: dform.toml is read
/// after the backend is chosen, so the launcher finds a provider's grants
/// here by the path it starts ([`grants_for`]).
static GRANTS: Mutex<BTreeMap<PathBuf, Grants>> = Mutex::new(BTreeMap::new());

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Record the grants of providers by their sources (`--provider` specs,
/// as [`crate::project::Manifest::provider_source`] gives them): a
/// directory source is recorded as the plugin it resolves to.
pub fn register(grants: impl IntoIterator<Item = (String, Grants)>) {
    let mut g = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    for (spec, grant) in grants {
        let path = match super::source::resolve(&spec) {
            super::source::Source::Plugin(p) => p,
            super::source::Source::Mock(_) => continue,
        };
        g.insert(canonical(&path), grant);
    }
}

/// The grants recorded for the provider at `path`; none when dform.toml
/// names it nowhere (`provider check PATH` outside a project), named after
/// its file.
pub fn grants_for(path: &Path) -> Grants {
    let g = GRANTS.lock().unwrap_or_else(|e| e.into_inner());
    g.get(&canonical(path)).cloned().unwrap_or_else(|| {
        Grants::none(
            &path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
        )
    })
}

/// What `provider check` prints about how a provider is hosted: which
/// host ran it, its manifest and grants. The launcher records it for the
/// program it started ([`observe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosting {
    /// `wasm` or `native`.
    pub host: &'static str,
    pub manifest: Option<Manifest>,
    pub grants: Grants,
}

static HOSTED: Mutex<BTreeMap<String, Hosting>> = Mutex::new(BTreeMap::new());

/// Record how the provider `program` (a link's `program`) is hosted.
pub fn observe(program: &str, h: Hosting) {
    HOSTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(program.to_string(), h);
}

/// How the provider `program` is hosted, if a launcher said.
pub fn hosting(program: &str) -> Option<Hosting> {
    HOSTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(program)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(allow: &[&str], credentials: &[&str]) -> Grants {
        Grants {
            provider: "k8s".into(),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            credentials: credentials.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// `wasi:sockets` is refused without a grant, naming the provider and
    /// the package; with one it is admitted. The host's own interfaces
    /// need none.
    #[test]
    fn a_grantable_import_needs_its_grant() {
        let m = Manifest::of(
            [
                "dform:host/http@1.0.0",
                "wasi:sockets/tcp@0.2.12",
                "wasi:cli/stdout@0.2.12",
            ],
            true,
        );
        let e = grants(&[], &[]).admit(&m).unwrap_err();
        assert!(e.contains("provider k8s imports wasi:sockets"), "{e}");
        assert!(e.contains("allow = [\"wasi:sockets\"]"), "{e}");
        assert!(grants(&["wasi:sockets"], &[]).admit(&m).is_ok());
        assert!(
            grants(&[], &[])
                .admit(&Manifest::of(["dform:host/ssh"], true))
                .is_ok()
        );
    }

    #[test]
    fn a_credential_not_granted_is_refused_naming_both() {
        let g = grants(&[], &["kubeconfig:prod"]);
        assert!(g.credential("kubeconfig:prod").is_ok());
        let e = g.credential("kubeconfig:staging").unwrap_err();
        assert_eq!(e.class, Class::Final);
        assert!(
            e.message
                .contains("provider k8s is not granted the credential kubeconfig:staging"),
            "{}",
            e.message
        );
    }
}
