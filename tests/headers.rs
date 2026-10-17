//! A module's `//!` header is a statement of what it is and does now
//! (dform-landing-lessons: no history unless it explains a shape). A
//! word about what it does NOT do yet, or about its maturity, rots
//! silently as the module grows: `cargo grep` for one once was the only
//! guard. This test is the guard CI runs.

use crate::common::repo;
use std::path::{Path, PathBuf};

/// Words a header rots around: a maturity claim ("prototype", "MVP", "in
/// progress", "TODO", "for now", "temporary") or a claim about what the
/// module does not do ("nothing here touches"), case-insensitive.
const ROT: &[&str] = &[
    "prototype",
    "mvp",
    "in progress",
    "nothing here touches",
    "todo",
    "for now",
    "temporary",
];

/// Every `.rs` file under `dir`, recursively (`target/` and other build
/// output skipped: this walks source trees named by the caller only).
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The module header at the top of `path`: its leading `//!` lines.
fn header(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `.rs` file's module header under `src/` and `crates/*/src`, by
/// its path from the repository root.
fn headers() -> Vec<(String, String)> {
    let root = repo();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    if let Ok(crates) = std::fs::read_dir(root.join("crates")) {
        for c in crates.flatten() {
            let src = c.path().join("src");
            if src.is_dir() {
                rust_files(&src, &mut files);
            }
        }
    }
    files
        .into_iter()
        .filter_map(|f| {
            let h = header(&f);
            (!h.is_empty()).then(|| {
                let rel = f.strip_prefix(root).unwrap_or(&f);
                (rel.display().to_string(), h)
            })
        })
        .collect()
}

/// No module header under `src/` or `crates/*/src` names a rot word
/// (`ROT`): one that does is either stale (fix the header) or genuinely
/// temporary (say so some other way than a word this test watches for).
/// No skip-list: a header that trips this on a branch stays failing
/// until its owner rewords it.
#[test]
fn no_module_header_rots() {
    let mut hits = Vec::new();
    for (path, text) in headers() {
        let lower = text.to_lowercase();
        for word in ROT {
            if lower.contains(word) {
                hits.push(format!("{path}: {word:?}"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "module headers with a rot word (reword the claim to what the module does now):\n{}",
        hits.join("\n")
    );
}
