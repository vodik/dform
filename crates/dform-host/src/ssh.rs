//! `dform:host/ssh`: the host's SSH client, behind [`Ssh`], for what a
//! provider's apply does on a host: `exec` (the one place a command runs,
//! R-151), `write`, `forward`. A host's file is read through `files`
//! (`ssh://USER@HOST/PATH`, `dform_core::files::ssh`, R-153), not here.
//!
//! TODO: wire these to `dform_core::files::ssh`'s session (its host-key
//! check against the run's known hosts, its key order): exec with argv
//! (quoted for the remote shell, which SSH's exec request always goes
//! through) and stdin; SFTP write with a mode; a direct-tcpip channel
//! behind a local listener for `forward`. No provider's apply calls them
//! yet; until one does every call is refused naming this.

use dform_core::plugin::host::{Endpoint, Error, Failure, Run, Target};
use std::net::SocketAddr;
use std::sync::OnceLock;

/// An SSH client as the host uses it.
pub trait Ssh: Send + Sync {
    /// Run `argv` on `on`; a host that does not answer yet is
    /// `Failure::NotYet`.
    fn exec(&self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure>;
    fn write(&self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error>;
    /// Forward a local listener through `via` to `to`: where the host's
    /// HTTP client connects for a request sent `via` the tunnel. The
    /// address is the host's; a provider gets only a handle.
    fn forward(&self, via: &Target, to: &Endpoint) -> Result<SocketAddr, Error>;
}

/// Makes the SSH client each provider's host uses.
pub type Factory = fn() -> Box<dyn Ssh>;

static CLIENT: OnceLock<Factory> = OnceLock::new();

/// Use `factory`'s clients for every provider started after this.
pub fn wire(factory: Factory) {
    let _ = CLIENT.set(factory);
}

/// A client: the wired one, else [`Unwired`].
pub fn client() -> Box<dyn Ssh> {
    match CLIENT.get() {
        Some(f) => f(),
        None => Box::new(Unwired),
    }
}

/// No SSH client is wired in this build.
pub struct Unwired;

fn unwired(on: &Target) -> Error {
    Error::fatal(format!(
        "ssh to {}@{}: this dform's host has no SSH client wired for a provider's exec, \
         write or forward yet (a host's file is read through files.read, \
         `ssh://USER@HOST/PATH`)",
        on.user, on.host
    ))
}

impl Ssh for Unwired {
    fn exec(&self, on: &Target, _: &[String], _: Option<&[u8]>) -> Result<Run, Failure> {
        Err(unwired(on).into())
    }

    fn write(&self, on: &Target, _: &str, _: &[u8], _: u32) -> Result<(), Error> {
        Err(unwired(on))
    }

    fn forward(&self, via: &Target, _: &Endpoint) -> Result<SocketAddr, Error> {
        Err(unwired(via))
    }
}
