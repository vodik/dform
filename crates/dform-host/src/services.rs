//! The host's calls for one provider ([`Services`]): `dform:host` served
//! with that provider's grants. Both transports serve it: the gRPC `Host`
//! service (`grpc`) and the component imports (`wasm`).

use crate::{git, http, ratelimit, ssh};
use dform_core::plugin::credentials::{self, Credential};
use dform_core::plugin::host::{
    Calls, Endpoint, Error, Failure, GitFile, Grants, Handle, HttpRequest, HttpResponse, Level,
    Opened, Run, Target,
};
use dform_core::plugin::timed;
use dform_core::timing::location;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// One provider's host: its grants and clients, which its calls only
/// read, and the credentials it opened and the tunnels it forwarded, by
/// handle. Its calls run at once (R-142): the handles are locked only to
/// insert or look one up, never across a call's I/O.
pub struct Services {
    grants: Grants,
    opened: Mutex<Held>,
    /// The pace of its requests, a window per account and bucket
    /// (`ratelimit`).
    limiter: Arc<ratelimit::Limiter>,
    ssh: Box<dyn ssh::Ssh>,
    git: git::Git,
}

/// What a provider's calls opened, by handle.
struct Held {
    credentials: BTreeMap<Handle, Arc<Credential>>,
    tunnels: BTreeMap<Handle, (Endpoint, SocketAddr)>,
    next: Handle,
}

impl Held {
    fn handle(&mut self) -> Handle {
        let h = self.next;
        self.next += 1;
        h
    }
}

impl Services {
    /// The host of the provider `grants` names.
    pub fn new(grants: Grants) -> Services {
        let limiter = Arc::new(ratelimit::Limiter::new(&grants.provider));
        Services::paced(grants, limiter)
    }

    /// The host of the provider `grants` names, its requests paced by
    /// `limiter`, which the provider's Configure answers tell each
    /// session's account (`ratelimit::Paced`).
    pub fn paced(grants: Grants, limiter: Arc<ratelimit::Limiter>) -> Services {
        Services {
            limiter,
            grants,
            opened: Mutex::new(Held {
                credentials: BTreeMap::new(),
                tunnels: BTreeMap::new(),
                next: 1,
            }),
            ssh: ssh::client(),
            git: git::Git::cache(),
        }
    }

    /// The same, with `ssh` as its SSH client (tests).
    pub fn with_ssh(mut self, ssh: Box<dyn ssh::Ssh>) -> Services {
        self.ssh = ssh;
        self
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    fn opened(&self) -> std::sync::MutexGuard<'_, Held> {
        // Each use is one insert or lookup: a panic leaves the maps whole.
        self.opened.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The credential and the tunnel's end the handles `auth` and `via`
    /// name.
    fn held(
        &self,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<(Option<Arc<Credential>>, Option<SocketAddr>), Error> {
        let o = self.opened();
        let cred = match auth {
            None => None,
            Some(h) => Some(
                o.credentials
                    .get(&h)
                    .cloned()
                    .ok_or_else(|| Error::fatal(format!("no credential {h} is open")))?,
            ),
        };
        let via = match via {
            None => None,
            Some(h) => Some(
                o.tunnels
                    .get(&h)
                    .map(|(_, a)| *a)
                    .ok_or_else(|| Error::fatal(format!("no tunnel {h} is open")))?,
            ),
        };
        Ok((cred, via))
    }

    /// `req`, once its bucket's window admits it, sent with `cred`
    /// through `via`: an `io.call` span (`http`), the child of the
    /// provider call it is sent for while that call is traced, else of
    /// the span the host's thread is in. Its wait for the window is a
    /// `ratelimit.wait` span under it.
    // `parent` before `target`: tracing-attributes 0.1 takes a `parent`
    // after it for a second one.
    #[tracing::instrument(
        parent = &parent(req.call),
        target = "dform",
        level = "debug",
        name = "io.call",
        skip_all,
        fields(
            otel.name = %format_args!("{} {} {}", self.grants.provider, req.method, location(&req.url)),
            otel.kind = "client",
            kind = "http",
            provider = self.grants.provider.as_str(),
            addr = %location(&req.url),
            bucket = req.bucket.as_deref(),
            status = tracing::field::Empty,
        )
    )]
    fn request(
        &self,
        req: HttpRequest,
        cred: Option<&Credential>,
        via: Option<SocketAddr>,
    ) -> Result<HttpResponse, Error> {
        let admitted = self.limiter.acquire(req.session, req.bucket.as_deref());
        let x = http::exchange(req, cred, via);
        admitted.answered(&x);
        if let Ok(r) = &x.answer {
            tracing::Span::current().record("status", r.status);
        }
        x.answer
    }
}

/// The span a request sent for the call numbered `call` is under: that
/// call's while it is in flight, else the current one. Held while the
/// request's span is made, so the call's cannot end meanwhile.
fn parent(call: Option<u64>) -> tracing::Span {
    call.and_then(timed::call_span)
        .unwrap_or_else(tracing::Span::current)
}

impl Calls for Services {
    fn open(&self, name: &str) -> Result<Opened, Error> {
        self.grants.credential(name)?;
        let c = credentials::load(name).map_err(|e| {
            Error::fatal(format!(
                "provider {}: the credential {name}: {e:#}",
                self.grants.provider
            ))
        })?;
        let endpoint = c.endpoint.clone();
        let mut o = self.opened();
        let handle = o.handle();
        o.credentials.insert(handle, Arc::new(c));
        Ok(Opened { handle, endpoint })
    }

    fn send(
        &self,
        req: HttpRequest,
        auth: Option<Handle>,
        via: Option<Handle>,
    ) -> Result<HttpResponse, Error> {
        let (cred, via) = self.held(auth, via)?;
        self.request(req, cred.as_deref(), via)
    }

    fn io_read(&self, location: &str) -> Result<Vec<u8>, Failure> {
        // The run's reader (its known hosts, mirrors, declared schemes),
        // or, outside a run, one of the host's own.
        let files = self.grants.files.get();
        files.read_for(&self.grants, location)
    }

    fn io_read_versioned(&self, location: &str) -> Result<dform_core::files::Document, Failure> {
        let files = self.grants.files.get();
        files.read_for_document(&self.grants, location)
    }

    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        self.ssh.exec(on, argv, stdin)
    }

    fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error> {
        self.ssh.write(on, path, data, mode)
    }

    fn forward(&self, via: &Target, to: &Endpoint) -> Result<Handle, Error> {
        let local = self.ssh.forward(via, to)?;
        let mut o = self.opened();
        let h = o.handle();
        o.tunnels.insert(h, (to.clone(), local));
        Ok(h)
    }

    fn tunnel(&self, h: Handle) -> Option<Endpoint> {
        self.opened().tunnels.get(&h).map(|(e, _)| e.clone())
    }

    fn git_commit(
        &self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, Error> {
        self.git.commit(repo, branch, files, message)
    }

    fn log(&self, level: Level, message: &str) {
        let level = match level {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warning",
            Level::Error => "error",
        };
        eprintln!("provider {}: {level}: {message}", self.grants.provider);
    }
}
