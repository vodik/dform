//! The component host (the `wasm` feature): written in the next commit.

use anyhow::{Result, bail};
use dform_core::plugin::host::Grants;
use dform_core::plugin::link::Link;
use std::path::Path;

pub fn link(path: &Path, _: Grants) -> Result<Link> {
    bail!(
        "provider {}: the wasm host is not written yet",
        path.display()
    )
}
