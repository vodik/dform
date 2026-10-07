//! `cargo xtask TASK`, the repository's own tasks:
//!
//! - `man`: the manual, dform(1) and a page per command
//!   (dform-stack-list.1), into target/man (`$CARGO_TARGET_DIR/man`), and
//!   docs/man's fragments into docs/reference.md between their markers.
//!   `man --check` writes nothing and fails when docs/reference.md's
//!   copies are not the fragments (tests/man.rs runs the same check).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["man"] => man(false),
        ["man", "--check"] => man(true),
        _ => bail!("usage: cargo xtask man [--check]"),
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
    let dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target"))
        .join("man");
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
