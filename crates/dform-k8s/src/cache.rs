//! Where the provider keeps what it derived from a cluster (its OpenAPI
//! documents, the schema derived from them, a deployment's extension of
//! the static schema): a file each, at the path Configure's cache
//! directory gives it, kept from one run to the next. The next run asks
//! the cluster only what changed, and a Configure that is the program's
//! first of a run (`deferred`) has the CRDs the cluster served at the
//! last one.
//!
//! A wasm provider reads and writes them through `wasi:filesystem`, in
//! the host's working directory (the SDK's `filesystem` feature): the
//! grant it has already, for the language front-end that parses its
//! schema. A cache the host keeps for a provider (an interface of
//! `dform:host`) would take its place, and the grant with it.

use anyhow::{Context, Result};
use std::path::Path;

/// The text kept at `path`, if any.
pub fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Keep `text` at `path`, in place of what was.
pub fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}
