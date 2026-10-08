//! Helpers for the CLI tests: a scratch directory per test and a way to run
//! the `dform` binary inside it. Tests never touch the repository's `dform.state/`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// What an unattended apply says when it stops before a tick that adds
/// what its plan could not name (R-30).
pub const STOPPED: &str = "run apply again to plan them against the world as it now is";

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
        std::fs::write(s.dir.join("dform.toml"), "[project]\nedition = \"2026\"\n").unwrap();
        s
    }

    /// A scratch directory outside every project: plan runs, apply only
    /// with a world fixture (`dev --world`).
    pub fn new(name: &str) -> Self {
        Scratch::empty(std::env::temp_dir(), "dform-test", name)
    }

    /// An empty scratch directory under the build's per-test directory,
    /// `CARGO_TARGET_TMPDIR`, `KIND-PID-NAME-N` (a git fixture or a
    /// linked executable stays out of /tmp, and the build's directory is
    /// no project's).
    pub fn in_target(kind: &str, name: &str) -> Self {
        Scratch::empty(PathBuf::from(env!("CARGO_TARGET_TMPDIR")), kind, name)
    }

    /// A new empty directory under `root`, `KIND-PID-NAME-N`.
    fn empty(root: PathBuf, kind: &str, name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = root.join(format!("{kind}-{}-{name}-{n}", std::process::id()));
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

    /// The JSON file `rel` (the mock's world `w.json`, its state
    /// `w.state.json`, a plan).
    pub fn json(&self, rel: &str) -> serde_json::Value {
        serde_json::from_str(&self.read(rel)).unwrap()
    }

    /// Run `dform ARGS` with the scratch directory as the working directory.
    pub fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Run {
        self.run_in("", args)
    }

    /// Run `dform ARGS` in the scratch directory's subdirectory `rel`.
    pub fn run_in<S: AsRef<std::ffi::OsStr>>(&self, rel: &str, args: &[S]) -> Run {
        let dir = self.path(rel);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dform().args(yes(args)).current_dir(&dir).output().unwrap();
        self.plain(Run::from(out))
    }

    /// Run the command line `ARGS` over `backend`.
    pub fn run_on<S: AsRef<std::ffi::OsStr>>(&self, backend: Backend, args: &[S]) -> Run {
        let mut c = backend.command();
        self.plain(Run::from(
            c.args(yes(args)).current_dir(&self.dir).output().unwrap(),
        ))
    }

    /// `r`, after checking it says no compiler word its program did not
    /// write (R-129, R-109): what the compiler calls its parts (`extern`,
    /// `atom`, `stratum`, `lattice`, `null`, `scoped`, `head`, `body`,
    /// `lowering`) reaches a user only from a program that writes the
    /// word. Stderr only, the lines a diagnostic quotes from the source
    /// left out: stdout is the plan and the queries, whose JSON says
    /// `null`.
    #[track_caller]
    fn plain(&self, r: Run) -> Run {
        // A path under the repository or the scratch directory is the
        // machine's words, not a message's: a worktree named for its
        // ticket (`r209-scoped`) must not fail this check.
        let repo = env!("CARGO_MANIFEST_DIR");
        let scratch = self.dir.to_string_lossy();
        let said: Vec<&str> = r
            .stderr
            .lines()
            .filter(|l| !quotes_source(l))
            .flat_map(|l| compiler_words(&l.replace(repo, "").replace(scratch.as_ref(), "")))
            .collect();
        if said.is_empty() {
            return r;
        }
        let mut files = Vec::new();
        df_files(&self.dir, true, &mut files);
        let written: Vec<String> = files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .collect();
        let unwritten: Vec<&&str> = said
            .iter()
            .filter(|w| {
                !written
                    .iter()
                    .any(|t| !compiler_words(t).iter().all(|x| x != *w))
            })
            .collect();
        assert!(
            unwritten.is_empty(),
            "a message names {unwritten:?}, which the program does not:\n{}{}",
            r.stdout,
            r.stderr
        );
        r
    }
}

/// The words a compiler calls its parts, as `t` says them (a word on its
/// own, not part of an identifier).
pub fn compiler_words(t: &str) -> Vec<&'static str> {
    const WORDS: &[&str] = &[
        "extern", "externs", "atom", "atoms", "stratum", "strata", "lattice", "null", "nulls",
        "scoped", "head", "heads", "body", "bodies", "lowering", "lowered",
    ];
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    WORDS
        .iter()
        .copied()
        .filter(|w| {
            t.match_indices(w)
                .any(|(i, _)| !t[..i].ends_with(ident) && !t[i + w.len()..].starts_with(ident))
        })
        .collect()
}

/// A line of a diagnostic that quotes the program (`  3 │ resource ..`):
/// its words are the user's.
fn quotes_source(l: &str) -> bool {
    l.trim_start()
        .split_once('│')
        .is_some_and(|(n, _)| n.trim().chars().all(|c| c.is_ascii_digit()) && !n.trim().is_empty())
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
    /// The exit status (docs/reference.md "Exit status"); `None` when a
    /// signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl From<Output> for Run {
    fn from(o: Output) -> Self {
        Run {
            ok: o.status.success(),
            code: o.status.code(),
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

    /// A run that stopped (exit 5, R-147): a plan file or an approval
    /// applied what it showed and stopped before what it did not.
    #[track_caller]
    pub fn stopped(self) -> Self {
        assert_eq!(
            self.code,
            Some(5),
            "dform did not stop\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
        self
    }

    /// The `plan: ...` summary line, or `stack NAME is up to date`.
    pub fn summary(&self) -> &str {
        self.stdout
            .lines()
            .find(|l| l.starts_with("plan:") || l.ends_with(" is up to date"))
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
/// (`crates/dform-provider-*`, `crates/dform-direct`). `cargo test` and
/// `cargo test --workspace` build every default member's binaries before
/// any test runs, so the binary is there; a test never runs cargo itself
/// (a nested build doubled the suite's build load and took the workspace
/// lock twice). `cargo test --test X` builds only this package's
/// binaries: a missing sibling fails naming the command that builds it,
/// and one from another commit fails the provider handshake
/// (`plugin::link::check_build`).
pub fn exe(name: &str) -> String {
    let dform = Path::new(env!("CARGO_BIN_EXE_dform"));
    let path = dform.with_file_name(name);
    if !path.exists() {
        let package = SIBLINGS
            .iter()
            .find(|(bin, _)| *bin == name)
            .map_or(name, |&(_, package)| package);
        panic!(
            "{} is not built: tests run the binary cargo built beside dform; \
             build it with `cargo build --package {package}` (or run \
             `cargo test --workspace`)",
            path.display()
        );
    }
    path.to_str().unwrap().to_string()
}

/// `ARGS` with `--yes` after `apply` (once): a test applies with no
/// terminal to confirm on. A test of the confirmation runs `dform()` itself.
pub fn yes<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut done = args
        .iter()
        .any(|a| a.as_ref() == "--yes" || a.as_ref() == "-y");
    for a in args {
        out.push(a.as_ref().to_os_string());
        if !done && a.as_ref() == "apply" {
            out.push("--yes".into());
            done = true;
        }
    }
    out
}

/// `dform`. The mock provider it spawns is itself (`dform __provider
/// fake`).
pub fn dform() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dform"))
}

/// `cmd` run answering each `[y/N]` question in turn, its answers on stdin
/// (`DFORM_TEST_ANSWERS`): what it printed, stdout and stderr in order, up
/// to and with each question and after the last, and its exit code. What
/// a question reads off a terminal is exit_status.rs's (a pty).
pub fn answering(dir: &Path, mut cmd: Command, answers: &[&str]) -> (Vec<String>, i32) {
    use std::io::Write;
    let out = dir.join(format!(
        "answers-{}.out",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let file = std::fs::File::create(&out).unwrap();
    let mut child = cmd
        .env("DFORM_TEST_ANSWERS", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for a in answers {
        writeln!(input, "{a}").unwrap();
    }
    drop(input);
    let code = child.wait().unwrap().code().expect("dform exits");
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    let mut said: Vec<String> = text.split_inclusive("[y/N] ").map(str::to_string).collect();
    if text.is_empty() || text.ends_with("[y/N] ") {
        said.push(String::new());
    }
    assert_eq!(
        said.len(),
        answers.len() + 1,
        "{} questions for {} answers:\n{text}",
        said.len() - 1,
        answers.len()
    );
    (said, code)
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
    yes(&out)
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect()
}

/// `dform dev --world w.json ARGS p.df` in `s`: a command on the scratch
/// program `p.df` against the mock's world `w.json` ([`on`]).
pub fn mock(s: &Scratch, args: &[&str]) -> Run {
    s.run(&on("p.df", &["--world", "w.json"], args))
}

/// The facts of `pred` the core-text program `src` derives, as
/// `fmt_atom` prints them.
pub fn facts(src: &str, pred: &str) -> Vec<String> {
    let program = dform_core::parser::parse_program(src).unwrap_or_else(|e| panic!("{e}"));
    let (r, _) = dform_core::engine::eval(&program, &[]).unwrap();
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(dform_core::spell::atom)
        .collect()
}

/// The compile error of the program file `src` (after a blank first
/// line, so its lines number from 2).
pub fn error(src: &str) -> String {
    dform_core::parser::parse_file("t.df", &format!("\n{src}"))
        .map(|_| ())
        .unwrap_err()
        .to_string()
}

/// The `.df` files in `dir`, sorted, and with `recurse` those of its
/// subdirectories.
pub fn df_files(dir: &Path, recurse: bool, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if recurse {
                df_files(&p, true, out);
            }
        } else if p.extension().is_some_and(|e| e == "df") {
            out.push(p);
        }
    }
}

/// Every `.df` file the repository ships (its root's, the examples', the
/// mock's schemas, the fixtures) and the positive corpus `tests/syntax/ok`:
/// what the parser, the formatter and the editor grammar are tested on.
pub fn corpus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    df_files(repo(), false, &mut out);
    for d in [
        "examples",
        "crates/dform-mock/schemas",
        "tests/fixtures",
        "tests/syntax/ok",
    ] {
        df_files(&repo().join(d), true, &mut out);
    }
    out
}

/// `p` relative to the repository root, as the tests name a file.
pub fn rel(p: &Path) -> String {
    p.strip_prefix(repo()).unwrap().display().to_string()
}

/// Compare `got` against the golden file `path`, or write it when
/// `UPDATE_GOLDEN=1`; `test` names the test target that accepts it.
#[track_caller]
pub fn golden_file(path: &Path, got: &str, test: &str) {
    if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, got).unwrap();
        return;
    }
    let accept = format!("run `UPDATE_GOLDEN=1 cargo test --test {test}` to accept it");
    let want = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\n{accept}\n---\n{got}",
            path.display()
        )
    });
    assert_eq!(
        want,
        got,
        "\n{} does not match its golden file\n{accept} if this is the intended change",
        path.display()
    );
}

/// The state of the deployment whose checkpoint is `rel` in `s`, as a run
/// reads it: the checkpoint with the audit log's `state` entries after it
/// replayed (R-146). A run dform did not finish (killed, or chaos
/// `stop-after`) left its last calls' answers in the log alone.
#[allow(dead_code)]
pub fn replayed(s: &Scratch, rel: &str) -> serde_json::Value {
    let st = dform_core::store::Deployment::local(&s.path(rel), "test")
        .load_state()
        .unwrap();
    serde_json::to_value(st).unwrap()
}

/// The addresses the mock's state `w.state.json` in `s` maps.
pub fn identities(s: &Scratch) -> Vec<String> {
    replayed(s, "w.state.json")["resources"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// `dform dev --world w.json query GOAL p.df` in `s` ([`mock`]).
pub fn query(s: &Scratch, goal: &str) -> Run {
    mock(s, &["query", goal])
}

/// The controller's log lines without their `HH:MM:SS ` stamps and its
/// first line (`controller ...`).
pub fn controller_log(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l[9..].to_string())
        .filter(|l| !l.starts_with("controller "))
        .collect()
}

/// `git ARGS` in `dir`, hermetic: no global or system configuration, no
/// inherited repository, a fixed identity, and `env` (a fixed date makes
/// a commit's hash the same on every run).
fn git_run(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> std::io::Result<Output> {
    Command::new("git")
        .args([
            "-c",
            "user.name=dform",
            "-c",
            "user.email=dform@example.com",
        ])
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .envs(env.iter().copied())
        .output()
}

/// `git ARGS` in `dir` ([`git_run`]), which must succeed; its output,
/// trimmed.
pub fn git_with(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let out = git_run(dir, args, env).unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// [`git_with`] and no more environment.
pub fn git(dir: &Path, args: &[&str]) -> String {
    git_with(dir, args, &[])
}

/// `git ARGS` in `dir` when git is there and succeeds: a test that needs
/// it skips without.
pub fn try_git(dir: &Path, args: &[&str]) -> Option<String> {
    git_run(dir, args, &[])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The repository root, for programs and fixtures the tests read.
pub fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// An HTTP server on the loopback that answers a request only once
/// another is open at the same time: `200 pair` to both, or `503 alone`
/// to one that waited `budget` with none beside it. Two requests sent at
/// once both succeed only if nothing between the sender and the server
/// serializes them (R-142).
pub fn answers_in_pairs(budget: std::time::Duration) -> std::net::SocketAddr {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Condvar, Mutex};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let open = Arc::new((Mutex::new(0usize), Condvar::new()));
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let open = open.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let (n, cv) = &*open;
                let mut n = n.lock().unwrap();
                *n += 1;
                cv.notify_all();
                let (n, _) = cv.wait_timeout_while(n, budget, |n| *n < 2).unwrap();
                let body = if *n >= 2 { "200 pair" } else { "503 alone" };
                drop(n);
                let mut s = s;
                let _ = write!(
                    s,
                    "HTTP/1.1 {body}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    addr
}
