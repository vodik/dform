//! Helpers for the table tests: a scratch directory under the build's
//! target directory (git fixtures stay out of /tmp) and git.

#![allow(dead_code)]

use crate::common::Scratch;
use std::path::Path;
use std::process::Command;

/// A scratch directory under `CARGO_TARGET_TMPDIR`, removed on drop.
pub fn scratch(name: &str) -> Scratch {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("tables-{}-{name}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Its own project: under the build's directory it would be the
    // repository's (the git root) otherwise.
    std::fs::write(dir.join("dform.toml"), "[project]\nedition = \"2026\"\n").unwrap();
    Scratch::adopt(dir)
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository `name` in `s`, and a clone of it, `work`.
pub fn repo(s: &Scratch, name: &str) {
    git(&s.dir, &["init", "-q", "--bare", name]);
    git(&s.dir, &["clone", "-q", name, "work"]);
}

/// Commit `text` as `file` in `work` and push it to `branch`; its commit.
pub fn push(s: &Scratch, file: &str, text: &str, branch: &str) -> String {
    s.write(&format!("work/{file}"), text);
    let w = s.path("work");
    git(&w, &["add", file]);
    git(&w, &["commit", "-q", "-m", file]);
    git(
        &w,
        &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
    );
    git(&w, &["rev-parse", "HEAD"])
}
