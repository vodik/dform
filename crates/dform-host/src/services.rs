//! The host's calls for one provider ([`Services`]): `dform:host` served
//! with that provider's grants. Both transports serve it: the gRPC `Host`
//! service (`grpc`) and the component imports (`wasm`).

use crate::{git, http, ssh};
use dform_core::plugin::credentials::{self, Credential};
use dform_core::plugin::host::{
    Calls, Endpoint, Error, Failure, GitFile, Grants, Handle, HttpRequest, HttpResponse, Level,
    Opened, Run, Target,
};
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
        Services {
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

    /// The same, with git's mirrors under `dir` (tests).
    pub fn with_git_cache(mut self, dir: std::path::PathBuf) -> Services {
        self.git = git::Git::at(dir);
        self
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    fn opened(&self) -> std::sync::MutexGuard<'_, Held> {
        // Each use is one insert or lookup: a panic leaves the maps whole.
        self.opened.lock().unwrap_or_else(|e| e.into_inner())
    }
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
        let (cred, via) = {
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
            (cred, via)
        };
        http::send(req, cred.as_deref(), via)
    }

    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure> {
        self.ssh.exec(on, argv, stdin)
    }

    fn read(&self, on: &Target, path: &str) -> Result<Vec<u8>, Failure> {
        self.ssh.read(on, path)
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

    fn git_read(&self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, Error> {
        self.git.read(repo, rev, path)
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
