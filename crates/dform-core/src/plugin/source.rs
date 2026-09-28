//! Where a provider comes from. A provider source is a path to an
//! executable, or a directory holding one named `dform-provider` or
//! `dform-provider-*`. Anything else (a name, a schema `.df` file, a
//! directory with only a `schema.df`) is a schema the mock provider plays.

use std::path::{Path, PathBuf};

/// A provider executable's name, or its prefix; also the first word of its
/// handshake line.
pub const MAGIC: &str = "dform-provider";

/// Where a provider comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An executable speaking the protocol.
    Plugin(PathBuf),
    /// A schema (a name or a path) the mock provider plays.
    Mock(String),
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.extension().is_none_or(|e| e != "df")
        && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
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
        if is_executable(p) {
            return Source::Plugin(p.to_path_buf());
        }
        if p.is_dir() {
            if let Some(exe) = plugin_in(p) {
                return Source::Plugin(exe);
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
}
