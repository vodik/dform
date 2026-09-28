//! Helpers for the CLI tests: a scratch directory per test and a way to run
//! the `dform` binary inside it. Tests never touch the repository's `.dform/`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dform-test-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch { dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.path(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }

    /// Run `dform ARGS` with the scratch directory as the working directory.
    pub fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_dform"))
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap();
        Run::from(out)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct Run {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl From<Output> for Run {
    fn from(o: Output) -> Self {
        Run {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }
}

impl Run {
    #[track_caller]
    pub fn success(self) -> Self {
        assert!(
            self.ok,
            "dform failed\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }

    #[track_caller]
    pub fn failure(self) -> Self {
        assert!(
            !self.ok,
            "dform unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }

    /// The `plan: ...` summary line.
    pub fn summary(&self) -> &str {
        self.stdout
            .lines()
            .find(|l| l.starts_with("plan:"))
            .unwrap_or("")
    }
}

/// The repository root, for programs and fixtures the tests read.
pub fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}
