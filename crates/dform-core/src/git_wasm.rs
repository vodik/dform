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
