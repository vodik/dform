//! `cargo xtask install [--prefix DIR] [--features F]`: dform and every
//! built-in provider, installed where dform looks for them
//! (docs/reference.md "Installing"). A stopgap until providers come by
//! location: `cargo install` has no place for the files a program loads.
//!
//! Without `--prefix`, `cargo install --path . --locked` (into
//! `$CARGO_HOME/bin`, as cargo has it) and the providers into the user's
//! `$XDG_DATA_HOME/dform/providers` (`~/.local/share`'s by default). With
//! `--prefix DIR`, what a package's build calls (a PKGBUILD's, a Debian
//! rule's): dform built and copied to `DIR/bin`, the providers to
//! `DIR/lib/dform/providers`. dform is built with the wasm host (`wasm`)
//! and the features given; a provider built as a wasm provider is
//! installed as one, `dform-provider-NAME.wasm`, the others as their
//! executables, `dform-provider-NAME`.

use crate::{Component, cargo, target_dir};
use anyhow::{Context, Result, bail};
use dform::plugin::source::{self, Env};
use std::path::{Path, PathBuf};

/// How a built-in provider is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Build {
    /// As a wasm provider, `dform-provider-NAME.wasm`.
    Wasm,
    /// As its executable, `dform-provider-NAME`: a provider not built as
    /// a wasm provider.
    Native,
}

/// A built-in provider: its name and how it is installed.
#[derive(Debug, Clone, Copy)]
struct Provider(&'static str, Build);

/// Every built-in provider (a `crates/dform-provider-NAME` but the fake,
/// which tests run).
const PROVIDERS: [Provider; 5] = [
    Provider("k8s", Build::Wasm),
    Provider("ovh", Build::Wasm),
    Provider("postgres", Build::Native),
    Provider("tailscale", Build::Native),
    Provider("vault", Build::Native),
];

impl Provider {
    /// Its package, `dform-provider-NAME`.
    fn package(self) -> String {
        source::executable(self.0)
    }

    /// Its build, under `target`.
    fn built(self, target: &Path) -> PathBuf {
        match self.1 {
            Build::Wasm => Component::of(&self.package()).file(target),
            Build::Native => target.join("release").join(self.package()),
        }
    }

    /// Its file's name where dform looks for it.
    fn file(self) -> String {
        match self.1 {
            Build::Wasm => source::wasm(self.0),
            Build::Native => source::executable(self.0),
        }
    }

    /// The built-in providers installed as `build`.
    fn each(build: Build) -> impl Iterator<Item = Provider> {
        PROVIDERS.into_iter().filter(move |p| p.1 == build)
    }
}

/// `cargo xtask install`'s arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Install {
    /// `--prefix DIR`: a system install under DIR.
    prefix: Option<PathBuf>,
    /// `--features F` (comma- or space-separated, given more than once).
    features: Vec<String>,
}

impl Install {
    pub fn parse(mut args: &[&str]) -> Result<Install> {
        let mut install = Install::default();
        while let [flag, rest @ ..] = args {
            let (pair, rest) = match (flag.split_once('='), rest) {
                (Some((flag, value)), rest) => ((flag, value), rest),
                (None, [value, rest @ ..]) => ((*flag, *value), rest),
                (None, []) => bail!("install: {flag} needs a value"),
            };
            match pair {
                ("--prefix", dir) => install.prefix = Some(PathBuf::from(dir)),
                ("--features", f) => install.features.extend(
                    f.split([',', ' '])
                        .filter(|f| !f.is_empty())
                        .map(String::from),
                ),
                (flag, _) => bail!("install: unknown flag {flag}"),
            }
            args = rest;
        }
        Ok(install)
    }

    pub fn run(&self) -> Result<()> {
        // Where the providers go first: no build when it is nowhere.
        let dir = self.providers_dir()?;
        self.dform()?;
        let packages: Vec<String> = Provider::each(Build::Native)
            .map(Provider::package)
            .collect();
        let mut native = vec!["build", "--release", "--locked"];
        for p in &packages {
            native.extend(["-p", p]);
        }
        cargo(&native)?;
        for p in Provider::each(Build::Wasm) {
            Component::of(&p.package()).build()?;
        }
        for to in place(&target_dir(), &dir)? {
            println!("installed {}", to.display());
        }
        Ok(())
    }

    /// dform's features: the wasm host and those given.
    fn features(&self) -> String {
        let given = self.features.iter().map(String::as_str);
        let mut features = vec!["wasm"];
        features.extend(given.filter(|f| *f != "wasm"));
        features.join(",")
    }

    /// dform, installed by cargo; with `--prefix`, built and copied to
    /// `PREFIX/bin`.
    fn dform(&self) -> Result<()> {
        let features = self.features();
        let Some(prefix) = &self.prefix else {
            return cargo(&[
                "install",
                "--path",
                ".",
                "--locked",
                "--features",
                &features,
            ]);
        };
        cargo(&[
            "build",
            "--release",
            "--locked",
            "-p",
            "dform",
            "--bin",
            "dform",
            "--features",
            &features,
        ])?;
        let from = target_dir().join("release/dform");
        let to = prefix.join("bin/dform");
        copy(&from, &to)?;
        println!("installed {}", to.display());
        Ok(())
    }

    /// Where the providers go: `PREFIX/lib/dform/providers`, else the
    /// user's providers directory.
    fn providers_dir(&self) -> Result<PathBuf> {
        match &self.prefix {
            Some(prefix) => Ok(source::lib_providers(prefix)),
            None => Env::here()
                .user_providers()
                .context("install: no data directory: set XDG_DATA_HOME or HOME"),
        }
    }
}

/// The built providers, from `target`, copied into `dir` under the names
/// dform looks for; the copies.
fn place(target: &Path, dir: &Path) -> Result<Vec<PathBuf>> {
    PROVIDERS
        .iter()
        .map(|p| {
            let to = dir.join(p.file());
            copy(&p.built(target), &to)?;
            Ok(to)
        })
        .collect()
}

/// `from` copied to `to`, its permissions with it, `to`'s directory made.
/// The copy replaces `to` rather than writing into it: a running dform
/// or provider keeps the file it has open.
fn copy(from: &Path, to: &Path) -> Result<()> {
    let dir = to.parent().context("a file has a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let part = to.with_extension("part");
    std::fs::copy(from, &part)
        .with_context(|| format!("copy {} to {}", from.display(), part.display()))?;
    std::fs::rename(&part, to).with_context(|| format!("rename to {}", to.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dform::plugin::source::{Lookup, Place};

    /// A directory of the test's own under the target directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = target_dir()
            .join("tmp")
            .join(format!("xtask-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_flags() {
        let parse = |args: &[&str]| Install::parse(args);
        assert_eq!(parse(&[]).unwrap(), Install::default());
        let i = parse(&["--prefix", "/usr", "--features=otel", "--features", "a,b"]).unwrap();
        assert_eq!(i.prefix, Some(PathBuf::from("/usr")));
        assert_eq!(i.features(), "wasm,otel,a,b");
        assert_eq!(parse(&["--features", "wasm"]).unwrap().features(), "wasm");
        assert!(parse(&["--prefix"]).is_err());
        assert!(parse(&["--root", "/"]).is_err());
    }

    /// Without `--prefix` the providers go to the user's providers
    /// directory, which the lookup looks in after dform's own; with it,
    /// to `PREFIX/lib/dform/providers`, the lookup's for a dform in
    /// `PREFIX/bin`.
    #[test]
    fn the_providers_go_where_dform_looks() {
        let lib = Install {
            prefix: Some("/opt/d".into()),
            ..Default::default()
        };
        let lib = lib.providers_dir().unwrap();
        assert_eq!(lib, Path::new("/opt/d/lib/dform/providers"));
        let env = Env {
            exe: Some("/opt/d/bin/dform".into()),
            xdg_data_home: Some("/home/u/data".into()),
            ..Default::default()
        };
        let dirs = Lookup::of(&env).dirs().to_vec();
        let user = env.user_providers().unwrap();
        assert!(dirs.contains(&(Place::UserData, user)), "{dirs:?}");
        assert!(dirs.contains(&(Place::Lib, lib)), "{dirs:?}");
    }

    /// Each built provider is copied under the name dform looks for, and
    /// the lookup finds every built-in provider there, a wasm provider
    /// as one and an executable as one.
    #[cfg(unix)]
    #[test]
    fn every_provider_is_placed_where_the_lookup_finds_it() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = scratch("place");
        let (target, dir) = (scratch.join("target"), scratch.join("providers"));
        for p in PROVIDERS {
            let built = p.built(&target);
            std::fs::create_dir_all(built.parent().unwrap()).unwrap();
            std::fs::write(&built, b"\0asm").unwrap();
            std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let placed = place(&target, &dir).unwrap();
        assert_eq!(placed.len(), PROVIDERS.len());
        let found = Lookup::default()
            .with(Place::UserData, dir.clone())
            .installed();
        for p in PROVIDERS {
            assert_eq!(found.get(p.0), Some(&(Place::UserData, dir.join(p.file()))));
        }
        // Again, over what is there.
        assert_eq!(place(&target, &dir).unwrap(), placed);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The table names every provider the workspace builds, the fake
    /// apart, and a wasm provider only where its crate builds one.
    #[test]
    fn every_built_in_provider_is_installed() {
        let manifest = std::fs::read_to_string(crate::root().join("Cargo.toml")).unwrap();
        let mut crates: Vec<&str> = manifest
            .lines()
            .filter_map(|l| l.trim().strip_prefix("\"crates/dform-provider-"))
            .filter_map(|l| l.strip_suffix("\","))
            .filter(|name| *name != "fake")
            .collect();
        crates.sort();
        crates.dedup();
        let names: Vec<&str> = PROVIDERS.iter().map(|p| p.0).collect();
        assert_eq!(crates, names);
        for p in PROVIDERS {
            let manifest = crate::root().join(format!("crates/{}/Cargo.toml", p.package()));
            let manifest = std::fs::read_to_string(manifest).unwrap();
            let component = manifest.lines().any(|l| l.starts_with("component ="));
            assert_eq!(component, p.1 == Build::Wasm, "{}", p.0);
        }
    }
}
