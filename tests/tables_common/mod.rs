//! Helpers for the table tests: a scratch directory under the build's
//! target directory (git fixtures stay out of /tmp) and git.

#![allow(dead_code)]

use crate::common::Scratch;

/// A scratch directory under `CARGO_TARGET_TMPDIR`, removed on drop.
pub fn scratch(name: &str) -> Scratch {
    let s = Scratch::in_target("tables", name);
    // Its own project: under the build's directory it would be the
    // repository's (the git root) otherwise.
    s.write("dform.toml", "[project]\nedition = \"2026\"\n");
    s
}

pub use crate::common::git;

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
