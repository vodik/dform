//! Shared by the tests/inspection_*.rs files: run `dform` against a repo
//! program in a scratch dir, and pin output under tests/golden/inspection/
//! with the golden tests' accept command (`UPDATE_GOLDEN=1`).

#![allow(dead_code)]

use crate::common::{Scratch, repo};

/// `dform --file REPO/FILE --world w.json ARGS` in a fresh scratch dir:
/// stdout on success, panics with stderr otherwise.
pub fn dform(file: &str, args: &[&str]) -> String {
    let s = Scratch::new("inspection");
    let file = repo().join(file);
    let mut all = vec!["--file", file.to_str().unwrap(), "--world", "w.json"];
    all.extend(args);
    s.run(&all).success().stdout
}

/// Compare against tests/golden/inspection/NAME.txt, or write it when
/// `UPDATE_GOLDEN=1`.
#[track_caller]
pub fn golden(name: &str, got: &str) {
    let path = repo()
        .join("tests/golden/inspection")
        .join(format!("{name}.txt"));
    if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\nrun `UPDATE_GOLDEN=1 cargo test --test inspection_{}` to accept it\n---\n{got}",
            path.display(),
            name.split('_').next().unwrap()
        )
    });
    assert_eq!(
        want,
        got,
        "{} does not match its golden file",
        path.display()
    );
}
