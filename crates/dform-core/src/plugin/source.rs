//! Where a provider comes from. A provider source is a path to an
//! executable, or a directory holding one named `dform-provider` or
//! `dform-provider-*`; or a component (R-13b): a `.wasm` file, or a
//! directory holding `provider.wasm`, which the wasm host runs. Anything
//! else (a name, a schema `.df` file, a directory with only a `schema.df`)
//! is a schema the mock provider plays.

use std::path::{Path, PathBuf};

/// A provider executable's name, or its prefix; also the first word of its
/// handshake line.
pub const MAGIC: &str = "dform-provider";

/// A component's file name in a provider directory.
pub const COMPONENT: &str = "provider.wasm";

/// Where a provider comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An executable speaking the protocol, or a component ([`is_component`]).
    Plugin(PathBuf),
    /// A schema (a name or a path) the mock provider plays.
    Mock(String),
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.extension().is_none_or(|e| e != "df")
        && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Off unix (a provider built as a component links dform-core) nothing is
/// an executable provider.
#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

/// Whether `p` is a provider component: a `.wasm` file.
pub fn is_component(p: &Path) -> bool {
    p.is_file() && p.extension().is_some_and(|e| e == "wasm")
}

/// The provider executable in a directory, if it has one.
pub fn plugin_in(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == MAGIC || n.starts_with(&format!("{MAGIC}-")))
                && is_executable(p)
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

pub fn resolve(spec: &str) -> Source {
    let p = Path::new(spec);
    if spec.contains('/') {
        if is_executable(p) || is_component(p) {
            return Source::Plugin(p.to_path_buf());
        }
        if p.is_dir() {
            if let Some(exe) = plugin_in(p) {
                return Source::Plugin(exe);
            }
            if is_component(&p.join(COMPONENT)) {
                return Source::Plugin(p.join(COMPONENT));
            }
            return Source::Mock(p.join("schema.df").display().to_string());
        }
    }
    Source::Mock(spec.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_schema_files_are_mock_schemas() {
        assert_eq!(resolve("fake"), Source::Mock("fake".into()));
        assert_eq!(
            resolve("providers/k8s/schema.df"),
            Source::Mock("providers/k8s/schema.df".into())
        );
    }

    /// A `.wasm` file, or a directory holding `provider.wasm`, is a
    /// component the wasm host runs.
    #[test]
    fn a_wasm_file_or_a_directory_holding_one_is_a_component() {
        let dir = std::env::temp_dir().join(format!("dform-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("prov")).unwrap();
        std::fs::write(dir.join("p.wasm"), b"\0asm").unwrap();
        std::fs::write(dir.join("prov/provider.wasm"), b"\0asm").unwrap();
        let file = dir.join("p.wasm");
        assert_eq!(
            resolve(file.to_str().unwrap()),
            Source::Plugin(file.clone())
        );
        let d = dir.join("prov");
        assert_eq!(
            resolve(d.to_str().unwrap()),
            Source::Plugin(d.join(COMPONENT))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
