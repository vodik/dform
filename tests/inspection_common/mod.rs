//! Shared by the tests/inspection_*.rs files: run `dform` against a repo
//! program in a scratch dir, and pin output under tests/golden/inspection/
//! with the golden tests' accept command (`UPDATE_GOLDEN=1`).

#![allow(dead_code)]

use crate::common::{Scratch, repo};

/// `dform dev --world w.json ARGS REPO/FILE [K=V...]` in a fresh scratch
/// dir (TARGET is the file and the key values after it):
/// stdout on success (the repo's path stripped from the file names it
/// prints, so snapshots are portable), panics with stderr otherwise.
pub fn dform(target: &str, args: &[&str]) -> String {
    let s = Scratch::new("inspection");
    let mut target = target.split_whitespace();
    let file = repo().join(target.next().unwrap());
    let mut all = crate::common::on(file.to_str().unwrap(), &["--world", "w.json"], args);
    all.extend(target.map(String::from));
    s.run(&all)
        .success()
        .stdout
        .replace(&format!("{}/", repo().display()), "")
}

/// Compare against tests/golden/inspection/NAME.txt, or write it when
/// `UPDATE_GOLDEN=1`.
#[track_caller]
pub fn golden(name: &str, got: &str) {
    let path = repo()
        .join("tests/golden/inspection")
        .join(format!("{name}.txt"));
    let test = format!("inspection_{}", name.split('_').next().unwrap());
    crate::common::golden_file(&path, got, &test);
}
