//! `dform:host/ssh`: the host's SSH client, behind [`Ssh`]. The client
//! itself is the built-in ssh provider's (`dform_core::plugin::ssh`, keys
//! from the agent), wired in with [`wire`].
//!
//! TODO(landing, with the built-in ssh provider, `plugin/ssh.rs`): its
//! russh client is private to its externs (`ssh.read`, `ssh.run` with a
//! command string). Wiring it here needs from it: a session by host, user
//! and port with its host-key check; exec with argv (quoted for the remote
//! shell, which SSH's exec request always goes through) and stdin; SFTP
//! read and write with a mode; a direct-tcpip channel behind a local
//! listener for `forward`. Its "not yet" (no answer within
//! `CONNECT_TIMEOUT`, refused, no route; a path that does not exist) maps
//! to `Failure::NotYet`. Then `dform_host::ssh::wire(..)` with an adapter.
//! Until then every call is refused naming this.

use dform_core::plugin::host::{Endpoint, Error, Failure, Run, Target};
use std::net::SocketAddr;
use std::sync::OnceLock;

/// An SSH client as the host uses it.
pub trait Ssh: Send {
    /// Run `argv` on `on`; a host that does not answer yet is
    /// `Failure::NotYet`.
    fn exec(&mut self, on: &Target, argv: &[String], stdin: Option<&[u8]>) -> Result<Run, Failure>;
    /// Read `path` over SFTP; a file that does not exist yet is
    /// `Failure::NotYet`.
    fn read(&mut self, on: &Target, path: &str) -> Result<Vec<u8>, Failure>;
    fn write(&mut self, on: &Target, path: &str, data: &[u8], mode: u32) -> Result<(), Error>;
    /// Forward a local listener through `via` to `to`: where the host's
    /// HTTP client connects for a request sent `via` the tunnel. The
    /// address is the host's; a provider gets only a handle.
    fn forward(&mut self, via: &Target, to: &Endpoint) -> Result<SocketAddr, Error>;
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
        "ssh to {}@{}: this dform's host has no SSH client wired yet (the built-in ssh \
         provider's, at landing)",
        on.user, on.host
    ))
}

impl Ssh for Unwired {
    fn exec(&mut self, on: &Target, _: &[String], _: Option<&[u8]>) -> Result<Run, Failure> {
        Err(unwired(on).into())
    }

    fn read(&mut self, on: &Target, _: &str) -> Result<Vec<u8>, Failure> {
        Err(unwired(on).into())
    }

    fn write(&mut self, on: &Target, _: &str, _: &[u8], _: u32) -> Result<(), Error> {
        Err(unwired(on))
    }

    fn forward(&mut self, via: &Target, _: &Endpoint) -> Result<SocketAddr, Error> {
        Err(unwired(via))
    }
}
