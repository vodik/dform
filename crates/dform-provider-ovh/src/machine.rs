//! The files of the machine the native build reads: OVH's configuration
//! files (`config`) and the resolver's (`dns`). A component reads none:
//! it is granted no filesystem, so its credentials are the program's
//! (`use ovh { token }`) or the operator's the host signs with. What the
//! provider keeps from one run to the next is the host's (`api::kept`).

use std::path::Path;

/// The bytes of the file at `path`, if it can be read.
#[cfg(not(target_family = "wasm"))]
pub fn read(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

#[cfg(target_family = "wasm")]
pub fn read(_: &Path) -> Option<Vec<u8>> {
    None
}
