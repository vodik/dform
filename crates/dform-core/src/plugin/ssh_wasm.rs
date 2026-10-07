//! `plugin::ssh` where dform-core is built for wasm (a provider component
//! links dform-core, R-13b): the same names with no client. A component is
//! a provider, not the engine that answers `use ssh`, and russh's
//! tokio networking does not build for wasm.

use crate::ast::ExternFn;
use crate::state::{KnownHost, State};
use crate::value::Value;
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

/// `ssh.read(+host, +user, +path, -content: secret(string))`.
pub const READ: &str = "ssh.read";
/// `ssh.run(+host, +user, +command, -stdout: string)`.
pub const RUN: &str = "ssh.run";

#[derive(Debug, Default)]
pub struct Ssh;

/// No key: there is no SSH client in a component.
pub fn key_named(_: &crate::ast::Program) -> Result<Option<String>> {
    Ok(None)
}

impl Ssh {
    pub fn new(_: BTreeMap<String, KnownHost>, _: Option<String>) -> Ssh {
        Ssh
    }

    /// Refused: there is no SSH client in a component.
    pub fn answer(&self, f: &ExternFn, _: &[Value]) -> Option<Result<Vec<Vec<Value>>>> {
        (f.name == READ || f.name == RUN).then(|| {
            Err(anyhow!(
                "{}: no SSH client in a wasm build of dform-core",
                f.name
            ))
        })
    }

    pub fn keep(&self, _: &mut State) {}
}
