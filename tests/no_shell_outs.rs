//! No shell-outs (R-103, house rules): git, ssh, http and archives are
//! in-process libraries. A program runs only where the operator asked for
//! one or where dform starts itself, so the product's sources (every
//! crate's `src/`, not its tests, benches or build script) call
//! `Command::new` in exactly these places.

use std::path::{Path, PathBuf};

/// Each allowed `Command::new`, by file, and why.
const ALLOWED: &[(&str, &str)] = &[
    // The provider launcher: a provider is a program.
    ("crates/dform-grpc/src/spawn.rs", "the provider launcher"),
    // `sh -c SINK`: the operator configured a command.
    (
        "crates/dform-core/src/audit.rs",
        "the configured audit sink",
    ),
    // `dform __explain` in a scratch copy of the project at a commit
    // (`diff --since`): dform running itself.
    ("crates/dform-core/src/diff.rs", "dform's own executable"),
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn only_the_launcher_the_sink_and_dform_itself_start_programs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    for c in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        sources(&c.path().join("src"), &mut files);
    }
    let mut found = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        // A crate's own unit tests may run what they like.
        let product = text.split("#[cfg(test)]").next().unwrap_or_default();
        let rel = f.strip_prefix(root).unwrap().display().to_string();
        for (i, line) in product.lines().enumerate() {
            if line.contains("Command::new(") && !line.trim_start().starts_with("//") {
                found.push(format!("{rel}:{}", i + 1));
            }
        }
    }
    let unexpected: Vec<&String> = found
        .iter()
        .filter(|at| {
            !ALLOWED
                .iter()
                .any(|(f, _)| at.starts_with(&format!("{f}:")))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "a shell-out outside the provider launcher, the audit sink and dform itself \
         (use gix, russh or dform's HTTP client): {unexpected:?}"
    );
    for (f, why) in ALLOWED {
        assert!(
            found.iter().any(|at| at.starts_with(&format!("{f}:"))),
            "{f} ({why}) no longer starts a program: take it off the list"
        );
    }
}
