//! `git` where dform-core is built for wasm (a provider component links
//! dform-core, R-13b): the same names with no repository. A component is
//! a provider, not the engine that reads the program's repository, and
//! gix's file locking and mirrors do not build for wasm.

use std::path::Path;

pub fn head(_: &Path) -> Option<String> {
    None
}

pub fn modified(_: &Path) -> Vec<String> {
    Vec::new()
}

pub fn clean(_: &Path) -> bool {
    false
}

pub fn resolve(_: &Path, rev: &str) -> anyhow::Result<String> {
    anyhow::bail!("git {rev}: no git in a wasm build of dform-core")
}

pub fn show(_: &Path, commit: &str, rel: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("git {commit}:{rel}: no git in a wasm build of dform-core")
}

pub fn checkout(_: &Path, commit: &str, _: &Path) -> anyhow::Result<()> {
    anyhow::bail!("git {commit}: no git in a wasm build of dform-core")
}

/// The mirror directory of `url`: none, with no git.
pub fn mirror_dir(_: &str) -> Option<std::path::PathBuf> {
    None
}

use crate::plugin::host::{Error, GitFile};

pub struct Pipe {
    pub read: Box<dyn std::io::Read + Send>,
    pub write: Box<dyn std::io::Write + Send>,
}

pub enum Via<'a> {
    Https { headers: Vec<(String, String)> },
    Ssh(&'a dyn Fn(&str) -> Result<Pipe, Error>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub commit: String,
    pub data: Vec<u8>,
}

/// No repository is read in a wasm build.
pub struct Git;

fn none(what: &str) -> Error {
    Error::fatal(format!("git {what}: no git in a wasm build of dform-core"))
}

impl Git {
    pub fn cache() -> Git {
        Git
    }

    pub fn at(_: std::path::PathBuf) -> Git {
        Git
    }

    pub fn mirror(&self, url: &str) -> Result<std::path::PathBuf, Error> {
        Err(none(url))
    }

    pub fn read(&self, repo: &str, _: &str, _: &str) -> Result<Vec<u8>, Error> {
        Err(none(repo))
    }

    pub fn read_remote(&self, url: &str, _: &str, _: &str, _: Via<'_>) -> Result<Fetched, Error> {
        Err(none(url))
    }

    pub fn commit(&self, repo: &str, _: &str, _: Vec<GitFile>, _: &str) -> Result<String, Error> {
        Err(none(repo))
    }
}
