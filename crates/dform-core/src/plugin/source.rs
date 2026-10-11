//! Where a provider comes from. A provider source is a path to an
//! executable, or a directory holding one named `dform-provider` or
//! `dform-provider-*`; or a component (R-13b): a `.wasm` file, or a
//! directory holding `provider.wasm`, which the wasm host runs. Anything
//! else (a name, a schema `.df` file, a directory with only a `schema.df`)
//! is a schema the mock provider plays.
//!
//! A provider dform.toml names without a `source` is the executable
//! `dform-provider-NAME`, looked for in one order ([`Lookup`]): beside
//! dform's own executable (a release archive's), in the target directory
//! of the checkout dform was built in (`cargo install --path .`), on
//! `PATH`; an entry's own `path` wins over the lookup.

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

/// Where a provider's executable was found by the [`Lookup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    /// Beside dform's own executable: the providers a release bundles.
    Bundled,
    /// The target directory of the checkout dform was built in.
    Checkout,
    /// A directory on `PATH`.
    PathVar,
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Place::Bundled => "beside dform",
            Place::Checkout => "the checkout's target directory",
            Place::PathVar => "PATH",
        })
    }
}

/// The directories a provider's executable is looked for in, in order:
/// the first holding `dform-provider-NAME` wins.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    dirs: Vec<(Place, PathBuf)>,
}

impl Lookup {
    /// This process's: the directory of its executable, the checkout's
    /// target directory (the profile dform was built with first), and
    /// `PATH`'s directories.
    pub fn here() -> Lookup {
        let mut lookup = Lookup::default();
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            lookup = lookup.with(Place::Bundled, dir);
        }
        // dform-core is `crates/dform-core` in the checkout.
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2);
        let target = option_env!("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .or_else(|| checkout.map(|c| c.join("target")));
        let profiles = if cfg!(debug_assertions) {
            ["debug", "release"]
        } else {
            ["release", "debug"]
        };
        for (target, profile) in target.iter().flat_map(|t| profiles.map(|p| (t, p))) {
            lookup = lookup.with(Place::Checkout, target.join(profile));
        }
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            lookup = lookup.with(Place::PathVar, dir);
        }
        lookup
    }

    /// Also look in `dir`, after the directories before it.
    pub fn with(mut self, place: Place, dir: PathBuf) -> Lookup {
        self.dirs.push((place, dir));
        self
    }

    /// The provider `name`'s executable, `dform-provider-NAME`, and where
    /// it was found.
    pub fn find(&self, name: &str) -> Option<(Place, PathBuf)> {
        let file = executable(name);
        self.dirs
            .iter()
            .map(|(place, dir)| (*place, dir.join(&file)))
            .find(|(_, p)| is_executable(p))
    }

    /// Every provider executable found, by name, each where [`Lookup::find`]
    /// finds it.
    pub fn installed(&self) -> std::collections::BTreeMap<String, (Place, PathBuf)> {
        let mut found = std::collections::BTreeMap::new();
        for (place, dir) in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for p in entries.filter_map(|e| e.ok().map(|e| e.path())) {
                let name = p.file_name().and_then(|n| n.to_str());
                if let Some(name) = name.and_then(|n| n.strip_prefix(&format!("{MAGIC}-")))
                    && is_executable(&p)
                {
                    found.entry(name.to_string()).or_insert((*place, p.clone()));
                }
            }
        }
        found
    }
}

/// The provider `name`'s executable's file name: `dform-provider-NAME`.
pub fn executable(name: &str) -> String {
    format!("{MAGIC}-{name}")
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

    /// A provider's executable is found in the first directory holding
    /// it, in the lookup's order; a file that is not executable is passed
    /// over.
    #[cfg(unix)]
    #[test]
    fn the_first_directory_holding_the_executable_wins() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dform-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let place = |d: &str, exec: bool| {
            let p = dir.join(d);
            std::fs::create_dir_all(&p).unwrap();
            let exe = p.join("dform-provider-x");
            std::fs::write(&exe, "").unwrap();
            let mode = if exec { 0o755 } else { 0o644 };
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        let (bundled, checkout, path) = (place("a", true), place("b", true), place("c", true));
        let lookup = Lookup::default()
            .with(Place::Bundled, bundled.clone())
            .with(Place::Checkout, checkout.clone())
            .with(Place::PathVar, path.clone());
        let x = |d: &Path| d.join("dform-provider-x");
        assert_eq!(lookup.find("x"), Some((Place::Bundled, x(&bundled))));
        place("a", false);
        assert_eq!(lookup.find("x"), Some((Place::Checkout, x(&checkout))));
        std::fs::remove_file(x(&checkout)).unwrap();
        assert_eq!(lookup.find("x"), Some((Place::PathVar, x(&path))));
        assert_eq!(lookup.find("y"), None);
        assert_eq!(
            lookup.installed().into_iter().collect::<Vec<_>>(),
            vec![("x".to_string(), (Place::PathVar, x(&path)))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
