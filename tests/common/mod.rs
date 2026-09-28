//! Helpers for the CLI tests: a scratch directory per test and a way to run
//! the `dform` binary inside it. Tests never touch the repository's `dform.state/`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Scratch {
    pub dir: PathBuf,
    // Private: a Scratch is only made here, so it only ever owns (and on
    // drop deletes) a directory under a test root. A test once wrapped the
    // repository's own directory and deleted a worktree with it.
    _owned: (),
}

/// The only places a Scratch may live: the OS temp directory and the
/// build's per-test directory. Never the roots themselves.
fn under_test_root(dir: &Path) -> bool {
    let roots = [
        std::env::temp_dir(),
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
    ];
    roots
        .iter()
        .any(|r| dir != r.as_path() && dir.starts_with(r))
}

impl Scratch {
    /// A scratch project: the directory, with a `dform.toml`, so that
    /// runs in it keep state (in its `dform.state/`).
    pub fn project(name: &str) -> Self {
        let s = Scratch::new(name);
        std::fs::write(s.dir.join("dform.toml"), "").unwrap();
        s
    }

    /// A scratch directory outside every project: plan runs, apply only
    /// with a world fixture (`dev --world`).
    pub fn new(name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dform-test-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch::adopt(dir)
    }

    /// Take ownership of `dir`, which must be under a test root (the OS
    /// temp directory or `CARGO_TARGET_TMPDIR`); it is deleted on drop.
    pub fn adopt(dir: PathBuf) -> Self {
        assert!(
            under_test_root(&dir),
            "Scratch::adopt: {} is not under a test root; refusing to own it",
            dir.display()
        );
        Scratch { dir, _owned: () }
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

    /// Write the mock's world `rel` and, beside it (`<stem>.state.json`,
    /// where `--world` looks), the state that maps every object in it: a
    /// world dform made. A world without state is not dform's.
    pub fn write_owned_world(&self, rel: &str, world: &str) -> PathBuf {
        let w: serde_json::Value = serde_json::from_str(world).unwrap();
        let resources: serde_json::Map<String, serde_json::Value> = w["resources"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, o)| {
                let remote = o["name"].clone();
                (
                    k.clone(),
                    serde_json::json!({"provider": "fakecloud", "remote": remote}),
                )
            })
            .collect();
        let state = serde_json::json!({"version": 1, "resources": resources});
        let p = self.write(rel, world);
        std::fs::write(p.with_extension("state.json"), state.to_string()).unwrap();
        p
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }

    /// Run `dform ARGS` with the scratch directory as the working directory.
    pub fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Run {
        self.run_in("", args)
    }

    /// Run `dform ARGS` in the scratch directory's subdirectory `rel`.
    pub fn run_in<S: AsRef<std::ffi::OsStr>>(&self, rel: &str, args: &[S]) -> Run {
        let dir = self.path(rel);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dform().args(args).current_dir(&dir).output().unwrap();
        Run::from(out)
    }

    /// Run the command line `ARGS` over `backend`.
    pub fn run_on<S: AsRef<std::ffi::OsStr>>(&self, backend: Backend, args: &[S]) -> Run {
        let mut c = backend.command();
        Run::from(c.args(args).current_dir(&self.dir).output().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Checked again here: `dir` is public and could have been changed.
        if under_test_root(&self.dir) {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!(
                "Scratch: not deleting {}: not under a test root",
                self.dir.display()
            );
        }
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

    /// The `plan: ...` summary line, or `stack NAME is undeformed`.
    pub fn summary(&self) -> &str {
        self.stdout
            .lines()
            .find(|l| l.starts_with("plan:") || l.ends_with(" is undeformed"))
            .unwrap_or("")
    }
}

/// How a run reaches the mock: `dform` spawns it and speaks gRPC (the
/// process backend); `dform-direct` links it in (the direct backend), or
/// links it in and encodes and decodes every message through prost (the
/// wire backend).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Process,
    Direct,
    Wire,
}

pub const BACKENDS: [Backend; 3] = [Backend::Process, Backend::Direct, Backend::Wire];

impl Backend {
    pub fn command(self) -> Command {
        match self {
            Backend::Process => dform(),
            Backend::Direct | Backend::Wire => {
                let mut c = Command::new(exe("dform-direct"));
                c.env(
                    "DFORM_BACKEND",
                    if self == Backend::Wire {
                        "wire"
                    } else {
                        "direct"
                    },
                );
                c
            }
        }
    }
}

/// The packages whose binaries tests run beside `dform`, by binary.
const SIBLINGS: &[(&str, &str)] = &[
    ("dform-provider-fake", "dform-provider-fake"),
    ("dform-provider-k8s", "dform-provider-k8s"),
    ("dform-direct", "dform-direct"),
    ("dform-approve", "dform-direct"),
];

/// A binary cargo builds beside `dform` from a package of its own
/// (`crates/dform-provider-*`, `crates/dform-direct`). `cargo test --test X`
/// builds only this package's binaries, so a test would run a sibling as
/// stale as the last workspace build: the first ask for one in a test
/// process builds its package into the same target directory and profile.
pub fn exe(name: &str) -> String {
    let dform = Path::new(env!("CARGO_BIN_EXE_dform"));
    if let Some(&(_, package)) = SIBLINGS.iter().find(|(bin, _)| *bin == name) {
        build(dform, package);
    }
    dform.with_file_name(name).to_str().unwrap().to_string()
}

/// `dform`, with the mock provider it spawns (`dform-provider-fake`) built.
pub fn dform() -> Command {
    exe("dform-provider-fake");
    Command::new(env!("CARGO_BIN_EXE_dform"))
}

/// Build `package` once per test process, beside `dform`.
fn build(dform: &Path, package: &str) {
    static BUILT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if built.iter().any(|p| p == package) {
        return;
    }
    let profile_dir = dform.parent().unwrap();
    let profile = match profile_dir.file_name().unwrap().to_str().unwrap() {
        "debug" => "dev",
        p => p,
    };
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()));
    c.args([
        "build",
        "--quiet",
        "--package",
        package,
        "--profile",
        profile,
    ])
    .arg("--target-dir")
    .arg(profile_dir.parent().unwrap())
    .current_dir(env!("CARGO_MANIFEST_DIR"));
    // What cargo set for this test run, not for the build.
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if k.starts_with("CARGO_PKG_")
            || matches!(
                &*k,
                "CARGO_MANIFEST_DIR"
                    | "CARGO_MANIFEST_PATH"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_TARGET_TMPDIR"
            )
        {
            c.env_remove(&*k);
        }
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "cargo build --package {package}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    built.push(package.to_string());
}

/// Copy the directory `from` (a project) to `to`, recursively, but for its
/// `dform.state/`.
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let name = e.file_name();
        if name == "dform.state" {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).unwrap();
        }
    }
}

/// Commands that live under `dform dev`.
const DEV: &[&str] = &["strata", "graph", "eval", "show"];

/// A command on the program `file`: `dev` and the mock's flags `mock`
/// first (when there are any, or the command is a `dev` one), then `args`
/// (the command and its arguments), then `file`, the command's target.
/// `apply PLAN.json` and the commands that take no target get none.
pub fn on(file: &str, mock: &[&str], args: &[&str]) -> Vec<String> {
    let mut out: Vec<&str> = Vec::new();
    if !mock.is_empty() || args.first().is_some_and(|c| DEV.contains(c)) {
        out.push("dev");
        out.extend_from_slice(mock);
    }
    out.extend_from_slice(args);
    let planned = matches!(args, ["apply", f, ..] if f.ends_with(".json"));
    let untargeted = matches!(
        args,
        ["state", "taint", ..] | ["stack", "handover" | "rekey" | "list", ..]
    );
    if !planned && !untargeted {
        out.push(file);
    }
    out.into_iter().map(String::from).collect()
}

/// The repository root, for programs and fixtures the tests read.
pub fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}
