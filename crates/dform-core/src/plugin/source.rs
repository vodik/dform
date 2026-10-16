//! Where a provider comes from. A provider source is a path to an
//! executable, or a directory holding one named `dform-provider` or
//! `dform-provider-*`; or a component (R-13b): a `.wasm` file, or a
//! directory holding `provider.wasm`, which the wasm host runs. Anything
//! else (a name, a schema `.df` file, a directory with only a `schema.df`)
//! is a schema the mock provider plays.
//!
//! A provider dform.toml names without a `source` is the executable
//! `dform-provider-NAME` or the wasm provider `dform-provider-NAME.wasm`,
//! looked for in one order ([`Lookup::of`]; docs/reference.md
//! "Installing"): beside dform's own executable (a release archive's),
//! the user's data directory (`cargo xtask install`'s), the system's data
//! directories, the install's `lib` (a package's), the target directory
//! of the checkout dform was built in, `PATH`; an entry's own `path` wins
//! over the lookup.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
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

/// Where a provider was found by the [`Lookup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    /// Beside dform's own executable: the providers a release bundles.
    Bundled,
    /// The user's: `$XDG_DATA_HOME/dform/providers`, where `cargo xtask
    /// install` puts them.
    UserData,
    /// A `$XDG_DATA_DIRS` entry's `dform/providers`.
    SystemData,
    /// The install's `lib/dform/providers`, beside the `bin` holding
    /// dform (a package's, `$PREFIX/lib`), or `/usr/lib`'s.
    Lib,
    /// The target directory of the checkout dform was built in.
    Checkout,
    /// A directory on `PATH`.
    PathVar,
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Place::Bundled => "beside dform",
            Place::UserData => "XDG_DATA_HOME",
            Place::SystemData => "XDG_DATA_DIRS",
            Place::Lib => "the install's lib directory",
            Place::Checkout => "the checkout's target directory",
            Place::PathVar => "PATH",
        })
    }
}

/// A data or lib directory's providers directory.
const PROVIDERS: &str = "dform/providers";

/// What the [`Lookup`]'s directories are made of: dform's executable and
/// the variables naming the others.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// dform's own executable.
    pub exe: Option<PathBuf>,
    /// `$HOME`, `$XDG_DATA_HOME`, `$XDG_DATA_DIRS` and `$PATH`.
    pub home: Option<OsString>,
    pub xdg_data_home: Option<OsString>,
    pub xdg_data_dirs: Option<OsString>,
    pub path: Option<OsString>,
}

impl Env {
    /// This process's.
    pub fn here() -> Env {
        Env {
            exe: std::env::current_exe().ok(),
            home: std::env::var_os("HOME"),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            xdg_data_dirs: std::env::var_os("XDG_DATA_DIRS"),
            path: std::env::var_os("PATH"),
        }
    }

    /// The user's providers directory, `dform/providers` under the
    /// user's data directory: `$XDG_DATA_HOME`, else `~/.local/share`. A
    /// relative path is none (the XDG spec's rule).
    pub fn user_providers(&self) -> Option<PathBuf> {
        let data = absolute(self.xdg_data_home.as_deref())
            .or_else(|| Some(absolute(self.home.as_deref())?.join(".local/share")))?;
        Some(data.join(PROVIDERS))
    }

    /// The system's data directories: `$XDG_DATA_DIRS`'s absolute
    /// entries, else `/usr/local/share` and `/usr/share`.
    fn data_dirs(&self) -> Vec<PathBuf> {
        let dirs = self.xdg_data_dirs.as_deref().filter(|d| !d.is_empty());
        let dirs = dirs.unwrap_or("/usr/local/share:/usr/share".as_ref());
        std::env::split_paths(dirs)
            .filter(|d| d.is_absolute())
            .collect()
    }
}

/// The providers directory of an install at `prefix` (dform in
/// `PREFIX/bin`): `PREFIX/lib/dform/providers`.
pub fn lib_providers(prefix: &Path) -> PathBuf {
    prefix.join("lib").join(PROVIDERS)
}

/// `v` as a directory, when it is an absolute path.
fn absolute(v: Option<&OsStr>) -> Option<PathBuf> {
    v.map(PathBuf::from).filter(|p| p.is_absolute())
}

/// The directories a provider is looked for in, in order: the first
/// holding the provider `NAME`, its executable `dform-provider-NAME` or
/// else its wasm provider `dform-provider-NAME.wasm`, wins.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    dirs: Vec<(Place, PathBuf)>,
}

impl Lookup {
    /// This process's: [`Lookup::of`] [`Env::here`].
    pub fn here() -> Lookup {
        Lookup::of(&Env::here())
    }

    /// `env`'s, in order: the directory of dform's executable; the
    /// user's data directory's `dform/providers`; each system data
    /// directory's; the install's `lib/dform/providers` (the `lib` beside
    /// the `bin` holding dform) and `/usr/lib/dform/providers`; the
    /// target directory of the checkout dform was built in (the profile
    /// it was built with first); `PATH`'s directories.
    pub fn of(env: &Env) -> Lookup {
        let bin = env.exe.as_deref().and_then(Path::parent);
        let mut lookup = Lookup::default();
        if let Some(bin) = bin {
            lookup = lookup.with(Place::Bundled, bin.to_path_buf());
        }
        if let Some(dir) = env.user_providers() {
            lookup = lookup.with(Place::UserData, dir);
        }
        for data in env.data_dirs() {
            lookup = lookup.with(Place::SystemData, data.join(PROVIDERS));
        }
        if let Some(prefix) = bin.and_then(Path::parent) {
            lookup = lookup.with(Place::Lib, lib_providers(prefix));
        }
        lookup = lookup.with(Place::Lib, lib_providers(Path::new("/usr")));
        for dir in checkout_targets() {
            lookup = lookup.with(Place::Checkout, dir);
        }
        // An empty entry is the working directory, to a shell: never a
        // provider's.
        let path = std::env::split_paths(env.path.as_deref().unwrap_or_default());
        for dir in path.filter(|d| !d.as_os_str().is_empty()) {
            lookup = lookup.with(Place::PathVar, dir);
        }
        lookup
    }

    /// Also look in `dir`, after the directories before it; one looked
    /// in already is not looked in again.
    pub fn with(mut self, place: Place, dir: PathBuf) -> Lookup {
        if !self.dirs.iter().any(|(_, d)| *d == dir) {
            self.dirs.push((place, dir));
        }
        self
    }

    /// The directories, in order.
    pub fn dirs(&self) -> &[(Place, PathBuf)] {
        &self.dirs
    }

    /// The provider `name`'s executable or wasm provider, and where it
    /// was found.
    pub fn find(&self, name: &str) -> Option<(Place, PathBuf)> {
        self.dirs
            .iter()
            .find_map(|(place, dir)| Some((*place, provider_in(dir, name)?)))
    }

    /// Every provider found, by name, each where [`Lookup::find`] finds
    /// it.
    pub fn installed(&self) -> BTreeMap<String, (Place, PathBuf)> {
        let names: BTreeSet<String> = self
            .dirs
            .iter()
            .filter_map(|(_, dir)| std::fs::read_dir(dir).ok())
            .flatten()
            .filter_map(|e| provider_name(e.ok()?.file_name().to_str()?))
            .collect();
        names
            .into_iter()
            .filter_map(|name| Some((name.clone(), self.find(&name)?)))
            .collect()
    }
}

/// The profile directories of the target directory of the checkout
/// dform was built in, its own profile first.
fn checkout_targets() -> Vec<PathBuf> {
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
    target
        .iter()
        .flat_map(|t| profiles.map(|p| t.join(p)))
        .collect()
}

/// The provider `name` in `dir`: its executable, else its wasm provider.
fn provider_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let exe = dir.join(executable(name));
    if is_executable(&exe) {
        return Some(exe);
    }
    let wasm = dir.join(wasm(name));
    is_component(&wasm).then_some(wasm)
}

/// The provider a file in a providers directory is, by its name:
/// `NAME` of `dform-provider-NAME` or `dform-provider-NAME.wasm`.
fn provider_name(file: &str) -> Option<String> {
    let name = file.strip_prefix(MAGIC)?.strip_prefix('-')?;
    let name = name.strip_suffix(".wasm").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

/// The provider `name`'s wasm provider's file name:
/// `dform-provider-NAME.wasm`.
pub fn wasm(name: &str) -> String {
    format!("{}.wasm", executable(name))
}

/// The provider `name`'s executable's file name: `dform-provider-NAME`.
pub fn executable(name: &str) -> String {
    format!("{MAGIC}-{name}")
}

/// Whether the specs `a` and `b` name one file: a program's `source`
/// (`./providers/k8s`, from its file's directory) and dform.toml's
/// (`providers/k8s`, from the root) are spelled apart.
pub fn same_file(a: &str, b: &str) -> bool {
    match (Path::new(a).canonicalize(), Path::new(b).canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
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

    /// A provider is found in the first directory holding it, in the
    /// lookup's order: its executable, else its wasm provider; a file
    /// that is not executable is passed over.
    #[cfg(unix)]
    #[test]
    fn the_first_directory_holding_the_provider_wins() {
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
        let wasm = checkout.join("dform-provider-x.wasm");
        std::fs::write(&wasm, b"\0asm").unwrap();
        assert_eq!(lookup.find("x"), Some((Place::Checkout, x(&checkout))));
        std::fs::remove_file(x(&checkout)).unwrap();
        assert_eq!(lookup.find("x"), Some((Place::Checkout, wasm.clone())));
        assert_eq!(
            lookup.installed().into_iter().collect::<Vec<_>>(),
            vec![("x".to_string(), (Place::Checkout, wasm.clone()))]
        );
        std::fs::remove_file(&wasm).unwrap();
        assert_eq!(lookup.find("x"), Some((Place::PathVar, x(&path))));
        assert_eq!(lookup.find("y"), None);
        assert_eq!(
            lookup.installed().into_iter().collect::<Vec<_>>(),
            vec![("x".to_string(), (Place::PathVar, x(&path)))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The directories are beside dform, the user's data directory, each
    /// system data directory, the install's lib and `/usr/lib`, the
    /// checkout's target directory, `PATH`: each `dform/providers` but
    /// the first and the last two, a relative XDG directory none (the
    /// spec's rule), and one directory looked in once.
    #[test]
    fn the_lookup_order() {
        let p = PathBuf::from;
        let env = Env {
            exe: Some(p("/opt/dform/bin/dform")),
            home: Some("/home/u".into()),
            xdg_data_home: Some("/home/u/data".into()),
            xdg_data_dirs: Some("/srv/share:share:/usr/share".into()),
            path: Some("/usr/bin:/opt/dform/bin".into()),
        };
        let checkout = checkout_targets().into_iter().map(|d| (Place::Checkout, d));
        let expected: Vec<(Place, PathBuf)> = [
            (Place::Bundled, p("/opt/dform/bin")),
            (Place::UserData, p("/home/u/data/dform/providers")),
            (Place::SystemData, p("/srv/share/dform/providers")),
            (Place::SystemData, p("/usr/share/dform/providers")),
            (Place::Lib, p("/opt/dform/lib/dform/providers")),
            (Place::Lib, p("/usr/lib/dform/providers")),
        ]
        .into_iter()
        .chain(checkout.clone())
        .chain([(Place::PathVar, p("/usr/bin"))])
        .collect();
        assert_eq!(Lookup::of(&env).dirs(), expected);

        // Unset or relative, the XDG directories are their defaults;
        // a package's lib is /usr/lib.
        let env = Env {
            exe: Some(p("/usr/bin/dform")),
            home: Some("/home/u".into()),
            xdg_data_home: Some("data".into()),
            xdg_data_dirs: None,
            path: None,
        };
        let expected: Vec<(Place, PathBuf)> = [
            (Place::Bundled, p("/usr/bin")),
            (Place::UserData, p("/home/u/.local/share/dform/providers")),
            (Place::SystemData, p("/usr/local/share/dform/providers")),
            (Place::SystemData, p("/usr/share/dform/providers")),
            (Place::Lib, p("/usr/lib/dform/providers")),
        ]
        .into_iter()
        .chain(checkout)
        .collect();
        assert_eq!(Lookup::of(&env).dirs(), expected);
    }
}
