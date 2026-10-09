//! `DFORM_COMMIT`: the git commit dform is built from (`git rev-parse
//! --short HEAD`), else `unknown`; never a failed build without git. The
//! handshake's version is `plugin::backend::BUILD`.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
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
    // Build again when HEAD moves: HEAD itself (a checkout, a detached
    // commit), the branch it names (a commit), and packed-refs (a branch
    // with no loose ref); and their logs, which every commit appends to:
    // a branch whose ref was packed has no loose ref to watch until a
    // commit writes one, and HEAD names the branch without changing. In a
    // worktree HEAD and its log are the worktree's own.
    let mut watch = vec!["HEAD".to_string(), "packed-refs".to_string()];
    watch.push("logs/HEAD".to_string());
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch.push(format!("logs/{branch}"));
        watch.push(branch);
    }
    for p in watch {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", &p])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
