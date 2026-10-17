//! `DFORM_COMMIT`: the git commit dform is built from (`git rev-parse
//! --short HEAD`), else `unknown`; never a failed build without git. The
//! handshake's version is `plugin::backend::BUILD`.
//!
//! The rebuild trigger is this worktree's own ref state, never the
//! repository's shared one: a worktree's `HEAD` is its own file, but a
//! symbolic `HEAD` names a branch whose ref (and `packed-refs`, if the
//! branch has no loose ref) lives in the common dir and is shared by every
//! worktree. Watching `packed-refs` rebuilds this worktree whenever any
//! branch anywhere is packed or deleted, so it is read for the commit hash
//! but never watched. Only the worktree's `HEAD` and, when it is symbolic,
//! the loose ref file it names are watched; a branch that is only in
//! packed-refs has no loose file yet, so the stamp holds until a commit in
//! this worktree writes one.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn git_path(path: &str) -> Option<String> {
    git(&["rev-parse", "--path-format=absolute", "--git-path", path])
}

fn watch(path: &str) {
    if let Some(path) = git_path(path)
        && Path::new(&path).exists()
    {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = git(&["rev-parse", "--short", "HEAD"]);
    println!(
        "cargo:rustc-env=DFORM_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
    if commit.is_none() {
        return;
    }
    // This worktree's own HEAD: a checkout or a detached commit changes it.
    watch("HEAD");
    // A symbolic HEAD names a branch; watch only the loose ref file it
    // names, never packed-refs (shared, see module header).
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch(&branch);
    }
}
