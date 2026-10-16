//! The files of the machine the native build reads and writes: OVH's
//! configuration files (`config`) and dform's cache (`api`'s project ids
//! and clock offset). A component reads and writes none: it is granted no
//! filesystem, so its credentials are the program's (`use ovh { token }`)
//! and every run asks what a native one finds kept.

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

/// Write `bytes` to the file at `path`, its directory made: best effort.
#[cfg(not(target_family = "wasm"))]
pub fn write(path: &Path, bytes: &[u8]) {
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(path, bytes);
}

#[cfg(target_family = "wasm")]
pub fn write(_: &Path, _: &[u8]) {}
