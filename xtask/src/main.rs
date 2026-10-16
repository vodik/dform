//! `cargo xtask TASK`, the repository's own tasks:
//!
//! - `man`: the manual, dform(1) and a page per command
//!   (dform-stack-list.1), into target/man (`$CARGO_TARGET_DIR/man`), and
//!   docs/man's fragments into docs/reference.md between their markers.
//!   `man --check` writes nothing and fails when docs/reference.md's
//!   copies are not the fragments (tests/man.rs runs the same check).
//! - `components`: the components the wasm host's tests run
//!   (tests/host_wasm.rs), built for wasm32-wasip3 in release: the fake
//!   provider, the OVH and Kubernetes providers and the SDK's async-ABI
//!   probe (`examples/fetch_at_once`).
//! - `install [--prefix DIR] [--features F]`: dform and every built-in
//!   provider, installed where dform looks for them (install.rs).

mod install;

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["man"] => man(false),
        ["man", "--check"] => man(true),
        ["components"] => components(),
        ["install", args @ ..] => install::Install::parse(args)?.run(),
        _ => bail!(
            "usage: cargo xtask man [--check] | components | install [--prefix DIR] [--features F]"
        ),
    }
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ is in the repository")
}

fn man(check: bool) -> Result<()> {
    let reference = root().join("docs/reference.md");
    let text = std::fs::read_to_string(&reference)
        .with_context(|| format!("reading {}", reference.display()))?;
    let spliced = dform::man::splice(&text)?;
    let pages = dform::man::pages();
    if check {
        if spliced != text {
            bail!(
                "docs/reference.md's copies of docs/man/*.md differ from them: run `cargo xtask man`"
            );
        }
        return Ok(());
    }
    if spliced != text {
        std::fs::write(&reference, spliced)?;
        println!("docs/reference.md: the fragments copied");
    }
    let dir = target_dir().join("man");
    // The pages of commands since removed go too.
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    for page in &pages {
        std::fs::write(dir.join(format!("{}.1", page.name)), &page.roff)?;
    }
    println!("{}: {} pages", dir.display(), pages.len());
    Ok(())
}

/// The repository's target directory: `$CARGO_TARGET_DIR`, else
/// `target`.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target"))
}

/// `cargo ARGS` in the repository, the cargo running the xtask's.
fn cargo(args: &[&str]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(&cargo)
        .args(args)
        .current_dir(root())
        .status()
        .context("run cargo")?;
    if !status.success() {
        bail!("cargo {}: {status}", args.join(" "));
    }
    Ok(())
}

/// A component: a package's library built for wasm32-wasip3, or one of
/// its examples.
struct Component<'a> {
    package: &'a str,
    example: Option<&'a str>,
}

/// The components `components` builds.
const COMPONENTS: [Component<'static>; 4] = [
    Component::of("dform-provider-fake"),
    Component::of("dform-provider-ovh"),
    Component::of("dform-provider-k8s"),
    Component {
        package: "dform-sdk",
        example: Some("fetch_at_once"),
    },
];

impl<'a> Component<'a> {
    /// The package `package`'s library.
    const fn of(package: &'a str) -> Component<'a> {
        Component {
            package,
            example: None,
        }
    }

    /// A `cargo build` of the workspace's toolchain (rust-toolchain.toml
    /// carries the target), in release.
    fn build(&self) -> Result<()> {
        let mut args = vec![
            "build",
            "--release",
            "--target",
            "wasm32-wasip3",
            "--features",
            "component",
            "-p",
            self.package,
        ];
        if let Some(example) = self.example {
            args.extend(["--example", example]);
        }
        cargo(&args)
    }

    /// The built library's file: `NAME.wasm` (the package's name, `_` for
    /// `-`) in `target`'s wasm32-wasip3 release directory.
    fn file(&self, target: &Path) -> PathBuf {
        let release = target.join("wasm32-wasip3/release");
        match self.example {
            Some(example) => release.join(format!("examples/{example}.wasm")),
            None => release.join(format!("{}.wasm", self.package.replace('-', "_"))),
        }
    }
}

/// The components.
fn components() -> Result<()> {
    COMPONENTS.iter().try_for_each(Component::build)
}
