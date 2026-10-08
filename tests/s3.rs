//! The s3 state backend end to end: examples/demo planned and applied with
//! its state, plan key, audit log and lease in a bucket. Every test runs
//! against a fake S3 server in the test process (`dform_s3::fake`: S3's
//! ETag and conditional-write rules, no signatures), and again against a
//! real one when `DFORM_S3_TEST_ENDPOINT` names it; otherwise that run
//! says it is skipped. The real one's credentials are
//! `DFORM_S3_ACCESS_KEY_ID` / `DFORM_S3_SECRET_ACCESS_KEY`, its bucket
//! `DFORM_S3_TEST_BUCKET` (default `dform-test`, made when missing); each
//! test works under a fresh prefix and deletes it at the end. MinIO in
//! podman: `crates/dform-s3/minio.sh`.
//!
//!   eval "$(crates/dform-s3/minio.sh start)" && cargo test --test s3

mod common;
use common::{Run, Scratch, controller_log};
use dform_core::store::{LOCK, S3Spec, STATE, Store};
use dform_s3::S3Store;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The passphrase each deployment's master is sealed under (R-164): the
/// bucket never holds a master in the clear.
const PASSPHRASE: &str = "s3 test passphrase";

/// The lease the tests' projects take: short, so an expiry is quick.
const LEASE: Duration = Duration::from_secs(2);

struct Target {
    what: &'static str,
    endpoint: String,
    bucket: String,
    id: String,
    secret: String,
}

fn fake() -> &'static dform_s3::fake::Server {
    static SERVER: std::sync::OnceLock<dform_s3::fake::Server> = std::sync::OnceLock::new();
    SERVER.get_or_init(dform_s3::fake::Server::start)
}

/// The fake server, and the real one when there is one.
fn targets(test: &str) -> Vec<Target> {
    let mut out = vec![Target {
        what: "fake",
        endpoint: fake().endpoint.clone(),
        bucket: "dform-test".into(),
        id: "fake".into(),
        secret: "fake".into(),
    }];
    match std::env::var("DFORM_S3_TEST_ENDPOINT") {
        Ok(e) if !e.is_empty() => out.push(Target {
            what: "real",
            endpoint: e,
            bucket: std::env::var("DFORM_S3_TEST_BUCKET").unwrap_or_else(|_| "dform-test".into()),
            id: std::env::var("DFORM_S3_ACCESS_KEY_ID").expect("DFORM_S3_ACCESS_KEY_ID"),
            secret: std::env::var("DFORM_S3_SECRET_ACCESS_KEY")
                .expect("DFORM_S3_SECRET_ACCESS_KEY"),
        }),
        _ => eprintln!("{test}: the real S3 run is skipped (DFORM_S3_TEST_ENDPOINT is not set)"),
    }
    out
}

/// A copy of examples/demo whose stacks keep their state in `t`'s bucket
/// under a fresh prefix; the prefix is emptied when dropped.
struct Project<'a> {
    t: &'a Target,
    s: Scratch,
    prefix: String,
}

impl<'a> Project<'a> {
    fn new(t: &'a Target, name: &str) -> Project<'a> {
        Project::of(t, name, |s| {
            common::copy_dir(&common::repo().join("examples/demo"), &s.dir)
        })
    }

    /// A project `setup` writes, its stacks' state in the bucket.
    fn of(t: &'a Target, name: &str, setup: impl FnOnce(&Scratch)) -> Project<'a> {
        let s = Scratch::project(&format!("s3-{name}"));
        setup(&s);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let prefix = format!("dform-test-{}-{nanos:x}-{name}", std::process::id());
        let mut toml = std::fs::read_to_string(s.path("dform.toml")).unwrap();
        toml.push_str(&format!(
            "\n[defaults]\nbackend = 's3(\"{}\", \"{prefix}/{{stack}}\", \
             {{endpoint: \"{}\", region: \"us-east-1\"}})'\n\
             lease_duration = \"{}ms\"\nlease_renewal = \"500ms\"\n\n\
             [secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
            t.bucket,
            t.endpoint,
            LEASE.as_millis()
        ));
        std::fs::write(s.path("dform.toml"), toml).unwrap();
        let p = Project { t, s, prefix };
        p.bucket("").create_bucket().unwrap();
        p
    }

    /// The objects under `rel` in the project's prefix.
    fn bucket(&self, rel: &str) -> S3Store {
        let spec = S3Spec {
            bucket: self.t.bucket.clone(),
            prefix: self.prefix.clone(),
            endpoint: Some(self.t.endpoint.clone()),
            region: Some("us-east-1".into()),
        };
        S3Store::with_credentials(
            &spec,
            rel,
            rusty_s3::Credentials::new(&self.t.id, &self.t.secret),
        )
        .unwrap()
    }

    /// The deployment `dform[env=staging]`'s objects.
    fn store(&self) -> S3Store {
        self.bucket("dform/env=staging")
    }

    /// `s3(...)` of `rel` in the project's prefix, as a backend term.
    fn term(&self, rel: &str) -> String {
        format!(
            "s3(\"{}\", \"{}/{rel}\", {{endpoint: \"{}\", region: \"us-east-1\"}})",
            self.t.bucket, self.prefix, self.t.endpoint
        )
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = common::dform();
        c.args(common::yes(args))
            .current_dir(&self.s.dir)
            .env("DFORM_S3_ACCESS_KEY_ID", &self.t.id)
            .env("DFORM_S3_SECRET_ACCESS_KEY", &self.t.secret)
            .env("DFORM_TEST_PASSPHRASE", PASSPHRASE);
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Run {
        Run::from(self.command(args, &[]).output().unwrap())
    }

    fn spawn(&self, args: &[&str], env: &[(&str, &str)]) -> Child {
        self.command(args, env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// The state in the bucket.
    fn state(&self) -> serde_json::Value {
        let o = self
            .store()
            .get(STATE)
            .unwrap()
            .expect("state in the bucket");
        serde_json::from_slice(&o.bytes).unwrap()
    }

    /// The state as a run reads it: the checkpoint with the log's
    /// entries after it replayed (R-146), without the checkpoint's own
    /// fields.
    fn replayed(&self) -> serde_json::Value {
        let dep = dform_core::store::Deployment::new(
            std::sync::Arc::new(self.store()),
            "dform[env=staging]",
            dform_core::store::LeaseTimes::default(),
        );
        serde_json::to_value(dep.load_state().unwrap()).unwrap()
    }

    fn lease(&self) -> Option<serde_json::Value> {
        let o = self.store().get(LOCK).unwrap()?;
        Some(serde_json::from_slice(&o.bytes).unwrap())
    }
}

impl Drop for Project<'_> {
    fn drop(&mut self) {
        let all = self.bucket("");
        for k in all.list("").unwrap_or_default() {
            let _ = all.delete(&k);
        }
    }
}

const APPLY: &[&str] = &["apply", "dform", "env=staging"];
const PLAN: &[&str] = &["plan", "dform", "env=staging"];

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The output of a child that has exited (or been killed).
fn finish(c: Child) -> Run {
    Run::from(c.wait_with_output().unwrap())
}

fn signal(pid: u32, sig: &str) {
    let ok = Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill {sig} {pid}");
}

/// A directory for the stall hook (`DFORM_TEST_STALL_AT_WRITE`).
fn stall_dir(p: &Project, name: &str) -> String {
    let d = p.s.path(name);
    std::fs::create_dir_all(&d).unwrap();
    d.display().to_string()
}

#[test]
fn plan_and_apply_of_the_demo_keep_state_in_the_bucket() {
    for t in &targets("plan_and_apply_of_the_demo_keep_state_in_the_bucket") {
        let p = Project::new(t, "apply");
        let plan = p.run(PLAN).success();
        assert!(
            plan.stdout.contains("+ net.vpc main.vpc"),
            "{}: {}",
            t.what,
            plan.stdout
        );
        p.run(APPLY).success();
        let st = p.state();
        assert!(
            st["resources"].as_object().is_some_and(|r| r.len() > 5),
            "{}: {st}",
            t.what
        );
        assert_eq!(
            st["fence"], 1,
            "{}: the apply's lease fenced its writes",
            t.what
        );
        let again = p.run(PLAN).success();
        assert!(
            again.stdout.contains("is up to date"),
            "{}: {}",
            t.what,
            again.stdout
        );
        // Everything the local backend keeps is in the bucket, nothing of it
        // beside the project (the mock's world is the provider's, and stays).
        let keys = p.store().list("").unwrap();
        for k in [
            "state.json",
            "state.master",
            "state.audit/000001.jsonl",
            "state.lock",
        ] {
            assert!(keys.iter().any(|x| x == k), "{}: {k} in {keys:?}", t.what);
        }
        // The master is sealed, never a key file in the bucket (R-164).
        assert!(
            !keys.iter().any(|x| x == "state.key"),
            "{}: {keys:?}",
            t.what
        );
        // The audit log is in segments: an entry rewrites the last one only.
        assert!(
            !keys.iter().any(|x| x == "state.audit.jsonl"),
            "{}: {keys:?}",
            t.what
        );
        let local = p.s.path("dform.state/dform/env=staging");
        assert!(local.join("remote.json").exists(), "{}", t.what);
        for f in ["state.json", "state.key", "state.audit.jsonl", "state.lock"] {
            assert!(!local.join(f).exists(), "{}: {f} is local", t.what);
        }
        assert_eq!(p.lease().unwrap()["holder"], "", "{}: released", t.what);
        let show = p.run(&["state", "show", "dform", "env=staging"]).success();
        assert!(show.stdout.contains("s3://"), "{}: {}", t.what, show.stdout);
        let log = p.run(&["log", "verify", "dform[env=staging]"]).success();
        assert!(
            log.stdout.contains("the chain holds"),
            "{}: {}",
            t.what,
            log.stdout
        );
    }
}

#[test]
fn two_concurrent_applies_one_is_refused() {
    for t in &targets("two_concurrent_applies_one_is_refused") {
        let p = Project::new(t, "concurrent");
        let release = p.s.path("release");
        let a = p.spawn(
            APPLY,
            &[("DFORM_TEST_HOLD_LOCK", release.to_str().unwrap())],
        );
        wait_for("the first apply's lease", Duration::from_secs(30), || {
            p.lease().is_some_and(|l| l["holder"] != "")
        });
        // Held past its duration: it is renewed.
        std::thread::sleep(LEASE + Duration::from_millis(500));
        let b = p.run(APPLY).failure();
        assert!(
            b.stderr
                .contains("stack dform[env=staging] is locked by another apply"),
            "{}: {}",
            t.what,
            b.stderr
        );
        std::fs::write(&release, "").unwrap();
        let a = finish(a).success();
        assert!(
            !a.stdout.contains("apply: complete"),
            "{}: {}",
            t.what,
            a.stdout
        );
        assert_eq!(p.state()["fence"], 1, "{}", t.what);
    }
}

/// Start an apply that stops as it is about to make its `at`th state
/// write; returns it once it has stopped there.
fn stalled_apply(p: &Project, at: usize) -> (Child, String) {
    let dir = stall_dir(p, &format!("stall-{at}"));
    let a = p.spawn(
        APPLY,
        &[("DFORM_TEST_STALL_AT_WRITE", &format!("{at}:{dir}"))],
    );
    wait_for("the apply to stall", Duration::from_secs(60), || {
        Path::new(&dir).join("stalled").exists()
    });
    (a, dir)
}

/// State is a checkpoint of the audit log in a bucket too (R-146): an
/// apply killed (kill -9) after its in-flight record or a call's answer
/// is in the log and before the checkpoint after it, once its lease
/// expires, is taken over and resumed to the state an uninterrupted apply
/// leaves; the stale holder's entries are its own fence's, replayed.
#[test]
fn a_run_killed_between_the_log_and_the_checkpoint_recovers() {
    for t in &targets("a_run_killed_between_the_log_and_the_checkpoint_recovers") {
        let base = Project::new(t, "wal-base");
        base.run(APPLY).success();
        // Each project's deployment has a master of its own (R-163).
        let unmastered = |mut v: serde_json::Value| {
            v.as_object_mut().unwrap().remove("master");
            v
        };
        let want = unmastered(base.replayed());
        assert!(want.get("in_flight").is_none(), "{}: {want}", t.what);
        for at in ["logged:1", "logged:3"] {
            let p = Project::new(t, "wal-killed");
            let out = p
                .command(APPLY, &[("DFORM_TEST_ABORT_AT", at)])
                .output()
                .unwrap();
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(out.status.signal(), Some(9), "{} {at}: {out:?}", t.what);
            let expires = p.lease().unwrap()["expires_ms"].as_u64().unwrap();
            wait_for("the lease to expire", LEASE * 3, || {
                dform_core::store::now_ms() > expires + 100
            });
            let b = p.run(APPLY).success();
            assert!(
                !b.stdout.contains("apply: complete"),
                "{} {at}: {}",
                t.what,
                b.stdout
            );
            assert_eq!(unmastered(p.replayed()), want, "{} {at}", t.what);
            assert_eq!(p.state()["fence"], 2, "{} {at}", t.what);
            let again = p.run(PLAN).success();
            assert!(again.stdout.contains("is up to date"), "{} {at}", t.what);
            p.run(&["log", "verify", "dform[env=staging]"]).success();
        }
    }
}

#[test]
fn a_killed_holders_lease_expires_and_a_second_run_takes_over_and_resumes() {
    for t in &targets("a_killed_holders_lease_expires_and_a_second_run_takes_over_and_resumes") {
        let p = Project::new(t, "killed");
        // Killed after the tick began and one Apply call was written down.
        let (a, _) = stalled_apply(&p, 3);
        signal(a.id(), "-KILL");
        finish(a);
        assert!(p.state()["in_flight"].is_object(), "{}", t.what);
        // Its lease is live until it expires.
        let early = p.run(APPLY).failure();
        assert!(
            early.stderr.contains("is locked by another apply"),
            "{}: {}",
            t.what,
            early.stderr
        );
        let expires = p.lease().unwrap()["expires_ms"].as_u64().unwrap();
        wait_for("the lease to expire", LEASE * 3, || {
            dform_core::store::now_ms() > expires + 100
        });
        let b = p.run(APPLY).success();
        assert!(
            b.stderr.contains("taking over the lease of"),
            "{}: {}",
            t.what,
            b.stderr
        );
        assert!(
            b.stdout.contains(" remaining, resumed\n"),
            "{}: {}",
            t.what,
            b.stdout
        );
        assert!(
            !b.stdout.contains("apply: complete"),
            "{}: {}",
            t.what,
            b.stdout
        );
        let st = p.state();
        assert_eq!(st["fence"], 2, "{}", t.what);
        assert!(st.get("in_flight").is_none(), "{}: {st}", t.what);
        let plan = p.run(PLAN).success();
        assert!(
            plan.stdout.contains("is up to date"),
            "{}: {}",
            t.what,
            plan.stdout
        );
    }
}

#[test]
fn unlock_breaks_a_killed_holders_lease() {
    for t in &targets("unlock_breaks_a_killed_holders_lease") {
        let p = Project::new(t, "unlock");
        let (a, _) = stalled_apply(&p, 3);
        signal(a.id(), "-KILL");
        finish(a);
        let u = p
            .run(&["stack", "unlock", "dform", "env=staging"])
            .success();
        assert!(u.stdout.contains("is broken"), "{}: {}", t.what, u.stdout);
        let b = p.run(APPLY).success();
        assert!(
            b.stdout.contains(" remaining, resumed\n"),
            "{}: {}",
            t.what,
            b.stdout
        );
        let again = p
            .run(&["stack", "unlock", "dform", "env=staging"])
            .success();
        assert!(
            again.stdout.contains("is not locked"),
            "{}: {}",
            t.what,
            again.stdout
        );
    }
}

#[test]
fn a_stale_holders_state_write_is_refused_by_fencing() {
    for t in &targets("a_stale_holders_state_write_is_refused_by_fencing") {
        let p = Project::new(t, "stale");
        // A stops as it is about to write down its first Apply call, and
        // is paused (its renewer too) until its lease has expired.
        let (a, dir) = stalled_apply(&p, 2);
        signal(a.id(), "-STOP");
        let expires = p.lease().unwrap()["expires_ms"].as_u64().unwrap();
        wait_for("the lease to expire", LEASE * 3, || {
            dform_core::store::now_ms() > expires + 100
        });
        // B takes the lease over, and holds it before writing anything.
        let release = p.s.path("release");
        let b = p.spawn(
            APPLY,
            &[("DFORM_TEST_HOLD_LOCK", release.to_str().unwrap())],
        );
        wait_for(
            "B's lease and its fence write",
            Duration::from_secs(30),
            || p.state()["fence"] == 2,
        );
        let before = p.state();
        // A wakes up and writes: refused, and its write did not land.
        std::fs::write(Path::new(&dir).join("resume"), "").unwrap();
        signal(a.id(), "-CONT");
        let a = finish(a).failure();
        assert!(
            a.stderr.contains("refused by fencing"),
            "{}: {}",
            t.what,
            a.stderr
        );
        assert_eq!(p.state(), before, "{}: A's write did not land", t.what);
        // B goes on and finishes the apply.
        std::fs::write(&release, "").unwrap();
        let b = finish(b).success();
        assert!(
            !b.stdout.contains("apply: complete"),
            "{}: {}",
            t.what,
            b.stdout
        );
        let plan = p.run(PLAN).success();
        assert!(
            plan.stdout.contains("is up to date"),
            "{}: {}",
            t.what,
            plan.stdout
        );
    }
}

/// The server keeps the conditions every lease and state write relies on:
/// a server that ignored them would let two writers overwrite each other.
#[test]
fn the_server_refuses_a_conditional_write_over_another_version() {
    use dform_core::store::Cond;
    for t in &targets("the_server_refuses_a_conditional_write_over_another_version") {
        let p = Project::new(t, "conditions");
        let s = p.store();
        let e1 = s.put(STATE, b"one", &Cond::IfAbsent).unwrap();
        assert!(e1.is_some(), "{}", t.what);
        assert_eq!(
            s.put(STATE, b"two", &Cond::IfAbsent).unwrap(),
            None,
            "{}",
            t.what
        );
        let e2 = s
            .put(STATE, b"two", &Cond::IfMatch(e1.clone().unwrap()))
            .unwrap();
        assert!(e2.is_some(), "{}", t.what);
        assert_eq!(
            s.put(STATE, b"three", &Cond::IfMatch(e1.unwrap())).unwrap(),
            None,
            "{}",
            t.what
        );
        let o = s.get(STATE).unwrap().unwrap();
        assert_eq!((o.bytes, Some(o.etag)), (b"two".to_vec(), e2), "{}", t.what);
    }
}

/// `controller run` keeps its memo in the bucket beside the state.
#[test]
fn the_controller_runs_an_s3_stack() {
    for t in &targets("the_controller_runs_an_s3_stack") {
        let p = Project::new(t, "controller");
        let once = ["controller", "run", "--once", "dform", "env=staging"];
        let r = p.run(&once).success();
        assert_eq!(
            controller_log(&r.stdout).first().map(String::as_str),
            Some("event start"),
            "{}: {}",
            t.what,
            r.stdout
        );
        assert!(
            r.stdout
                .ends_with("stack dform[env=staging] is up to date\n"),
            "{}: {}",
            t.what,
            r.stdout
        );
        let keys = p.store().list("").unwrap();
        assert!(
            keys.iter().any(|k| k == "controller.json"),
            "{}: {keys:?}",
            t.what
        );
        assert!(
            !p.s.path("dform.state/dform/env=staging/controller.json")
                .exists(),
            "{}",
            t.what
        );
        let r = p.run(&once).success();
        assert_eq!(
            controller_log(&r.stdout),
            ["event resync", "stack dform[env=staging] is up to date"],
            "{}",
            t.what
        );
        // Its deployment is listed, with its last apply.
        let r = p.run(&["stack", "list"]).success();
        assert!(
            r.stdout
                .lines()
                .nth(1)
                .is_some_and(|l| l.contains("  dform[env=staging]  project.df  s3://")),
            "{}: {}",
            t.what,
            r.stdout
        );
    }
}

/// `stack rekey` moves a deployment's objects to the new key's prefix, and
/// its world with them.
#[test]
fn rekey_moves_an_s3_deployment_in_the_bucket() {
    for t in &targets("rekey_moves_an_s3_deployment_in_the_bucket") {
        let p = Project::new(t, "rekey");
        p.run(APPLY).success();
        let before = p.state();
        let r = p
            .run(&["stack", "rekey", "dform", "env=staging", "env=dev"])
            .success();
        assert!(
            r.stdout.contains(&format!(
                "stack dform[env=staging] rekeyed to dform[env=dev]: s3://{}/{}/dform/env=dev",
                t.bucket, p.prefix
            )),
            "{}: {}",
            t.what,
            r.stdout
        );
        assert_eq!(
            p.store().list("").unwrap(),
            Vec::<String>::new(),
            "{}: nothing is left behind",
            t.what
        );
        let moved = p.bucket("dform/env=dev");
        let keys = moved.list("").unwrap();
        for k in ["state.json", "state.master", "state.audit/000001.jsonl"] {
            assert!(keys.iter().any(|x| x == k), "{}: {k} in {keys:?}", t.what);
        }
        let now: serde_json::Value =
            serde_json::from_slice(&moved.get(STATE).unwrap().unwrap().bytes).unwrap();
        assert_eq!(now["resources"], before["resources"], "{}", t.what);
        assert!(p.s.path("dform.state/dform/env=dev/remote.json").exists());
        assert!(!p.s.path("dform.state/dform/env=staging").exists());
        let log = p.run(&["log", "verify", "dform[env=dev]"]).success();
        assert!(
            log.stdout.contains("the chain holds"),
            "{}: {}",
            t.what,
            log.stdout
        );
        let log = p.run(&["log", "dform[env=dev]"]).success();
        assert!(log.stdout.contains(" rekey "), "{}: {}", t.what, log.stdout);
    }
}

/// A deployment whose master is a local key file handed over to a bucket
/// (After R-164): refused without `[secrets]`, since a bucket never holds a
/// key file; with it, the key file is sealed first (the same master) and
/// the bucket gets `state.master`, never `state.key`.
#[test]
fn handover_to_a_bucket_seals_a_key_file() {
    let t = &targets("handover_to_a_bucket_seals_a_key_file")[0];
    let p = Project::new(t, "handover-key");
    let toml = p.s.read("dform.toml");
    let local = toml.split("\n[defaults]").next().unwrap().to_string();
    p.s.write("dform.toml", &local);
    p.run(APPLY).success();
    let key = p.s.path("dform.state/dform/env=staging/state.key");
    assert!(key.exists());
    let id = p.s.json("dform.state/dform/env=staging/state.json")["master"].clone();
    let to = p.term("handed");
    let r = p
        .run(&["stack", "handover", "dform[env=staging]", "--to", &to])
        .failure();
    assert!(
        r.stderr.contains("a bucket never holds one") && r.stderr.contains("[secrets] passphrase"),
        "{}",
        r.stderr
    );
    assert!(key.exists() && p.bucket("handed").list("").unwrap().is_empty());
    p.s.write(
        "dform.toml",
        &format!("{local}\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n"),
    );
    let r = p
        .run(&["stack", "handover", "dform[env=staging]", "--to", &to])
        .success();
    assert!(
        r.stderr
            .contains("its key file is sealed into state.master for the handover"),
        "{}",
        r.stderr
    );
    let objects = p.bucket("handed").list("").unwrap();
    assert!(
        objects.contains(&"state.master".to_string())
            && !objects.contains(&"state.key".to_string()),
        "{objects:?}"
    );
    let record: serde_json::Value = serde_json::from_slice(
        &p.bucket("handed")
            .get("state.master")
            .unwrap()
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(record["id"], id, "the same master: {record}");
    assert!(record["passphrase"].is_object(), "{record}");
    let r = p.run(PLAN).success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
    assert!(!r.stderr.contains("key file"), "{}", r.stderr);
}

/// `stack handover` moves a deployment between prefixes, and from a bucket
/// to a directory; the controller runs it where it is.
#[test]
fn handover_moves_an_s3_deployment_between_prefixes_and_to_local() {
    for t in &targets("handover_moves_an_s3_deployment_between_prefixes_and_to_local") {
        let p = Project::new(t, "handover");
        p.run(APPLY).success();
        let to = p.term("moved");
        let r = p
            .run(&["stack", "handover", "dform[env=staging]", "--to", &to])
            .success();
        assert!(
            r.stdout.contains(&format!(
                "handed over to {to}: s3://{}/{}/moved",
                t.bucket, p.prefix
            )),
            "{}: {}",
            t.what,
            r.stdout
        );
        assert_eq!(
            p.store().list("").unwrap(),
            Vec::<String>::new(),
            "{}",
            t.what
        );
        let registry = p.s.read("dform.state/stacks.json");
        assert!(
            registry.contains(&format!(
                "\"state\": \"s3://{}/{}/moved/state.json\"",
                t.bucket, p.prefix
            )),
            "{}: {registry}",
            t.what
        );
        let r = p.run(PLAN).success();
        assert!(
            r.stdout.contains("is up to date"),
            "{}: {}",
            t.what,
            r.stdout
        );
        let r = p.run(APPLY).failure();
        assert!(
            r.stderr.contains("the controller runs it"),
            "{}: {}",
            t.what,
            r.stderr
        );
        let r = p
            .run(&["controller", "run", "--once", "dform", "env=staging"])
            .success();
        assert_eq!(
            controller_log(&r.stdout),
            ["event start", "stack dform[env=staging] is up to date"],
            "{}",
            t.what
        );
        assert!(
            p.bucket("moved")
                .list("")
                .unwrap()
                .contains(&"controller.json".to_string()),
            "{}",
            t.what
        );
        // Out of the bucket: the world joins the state in the directory.
        p.run(&[
            "stack",
            "handover",
            "dform[env=staging]",
            "--to",
            "local(\"here\")",
        ])
        .success();
        assert_eq!(p.bucket("moved").list("").unwrap(), Vec::<String>::new());
        for f in [
            "state.json",
            "remote.json",
            "controller.json",
            "state.audit.jsonl",
        ] {
            assert!(p.s.path("here").join(f).exists(), "{}: {f}", t.what);
        }
        let r = p.run(PLAN).success();
        assert!(
            r.stdout.contains("is up to date"),
            "{}: {}",
            t.what,
            r.stdout
        );
        let log = p.run(&["log", "verify", "dform[env=staging]"]).success();
        assert!(
            log.stdout.contains("the chain holds"),
            "{}: {}",
            t.what,
            log.stdout
        );
    }
}

const PERSISTED: &str = r#"
use fake
extern kv.password(+name, -value)
resource db.user app {
  password = pw
} where kv.password("app", candidate), memo.first("app-pw", candidate, pw)
"#;

/// `secrets rotate` of a memo finds an s3 stack's state through its
/// program's backend.
#[test]
fn rotate_forgets_an_answer_in_the_bucket() {
    for t in &targets("rotate_forgets_an_answer_in_the_bucket") {
        let p = Project::of(t, "taint", |s| {
            s.write("stacks/p.df", PERSISTED);
            s.write(
                "providers/fake/schema.df",
                &(std::fs::read_to_string(
                    common::repo().join("crates/dform-mock/schemas/fake.df"),
                )
                .unwrap()
                    + "type_provider(db.user, \"fakecloud\")\n"),
            );
            s.write(
                "providers/fake/externs.df",
                "\nkv.password(\"app\", \"pw-first\")\n",
            );
        });
        p.run(&["apply", "p"]).success();
        let state = |p: &Project| {
            String::from_utf8(p.bucket("p").get(STATE).unwrap().unwrap().bytes).unwrap()
        };
        assert!(state(&p).contains("pw-first"), "{}", t.what);
        let r = p.run(&["secrets", "rotate", "p", "app-pw"]).success();
        assert!(
            r.stdout.contains(
                "  forgot what memo.first keeps: the next apply keeps its candidate\nrotated \
                 app-pw of p: generation 2"
            ),
            "{}: {}",
            t.what,
            r.stdout
        );
        assert!(!state(&p).contains("pw-first"), "{}", t.what);
    }
}

const NET: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
output vpc_cidr = "10.0.0.0/16"
output vpc_id = ref(net.vpc, "main", "id")
"#;

const APP: &str = r#"
use fake
use stacks.net as network
resource net.subnet a {
  cidr = network.vpc_cidr
  vpc_id = network.vpc_id
}
"#;

/// Another stack reads an s3 stack's outputs from the object it publishes
/// beside its state.
#[test]
fn another_stack_reads_an_s3_stacks_outputs() {
    for t in &targets("another_stack_reads_an_s3_stacks_outputs") {
        let p = Project::of(t, "outputs", |s| {
            s.write("stacks/net.df", NET);
            s.write("stacks/app.df", APP);
        });
        p.run(&["apply", "net"]).success();
        let published: serde_json::Value = serde_json::from_slice(
            &p.bucket("net")
                .get("outputs.json")
                .unwrap()
                .expect("published outputs")
                .bytes,
        )
        .unwrap();
        assert_eq!(published["deployment"], "net", "{}: {published}", t.what);
        let r = p.run(&["plan", "--why=none", "app"]).success();
        assert!(
            r.stdout.contains(
                "+ net.subnet[\"a\"]\n  cidr = \"10.0.0.0/16\"\n  vpc_id = \"net.vpc:main\"\n"
            ),
            "{}: {}",
            t.what,
            r.stdout
        );
    }
}

/// A stale holder that wakes after a takeover makes no provider call: the
/// lease is checked before each one is submitted.
#[test]
fn a_stale_holder_makes_no_provider_call() {
    for t in &targets("a_stale_holder_makes_no_provider_call") {
        let p = Project::new(t, "stale-submit");
        // A stops as it is about to submit its first Apply call, after
        // its last state write, and is paused until its lease expires.
        let dir = stall_dir(&p, "stall-submit");
        let a = p.spawn(
            APPLY,
            &[("DFORM_TEST_STALL_AT_SUBMIT", &format!("1:{dir}"))],
        );
        wait_for("the apply to stall", Duration::from_secs(60), || {
            Path::new(&dir).join("stalled").exists()
        });
        signal(a.id(), "-STOP");
        let expires = p.lease().unwrap()["expires_ms"].as_u64().unwrap();
        wait_for("the lease to expire", LEASE * 3, || {
            dform_core::store::now_ms() > expires + 100
        });
        let release = p.s.path("release");
        let b = p.spawn(
            APPLY,
            &[("DFORM_TEST_HOLD_LOCK", release.to_str().unwrap())],
        );
        wait_for("B's lease", Duration::from_secs(30), || {
            p.state()["fence"] == 2
        });
        let world = p.s.path("dform.state/dform/env=staging/remote.json");
        let objects = || -> usize {
            std::fs::read(&world)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|w| Some(w["resources"].as_object()?.len()))
                .unwrap_or(0)
        };
        assert_eq!(objects(), 0, "{}", t.what);
        std::fs::write(Path::new(&dir).join("resume"), "").unwrap();
        signal(a.id(), "-CONT");
        let a = finish(a).failure();
        assert!(
            a.stderr.contains("no provider call was made"),
            "{}: {}",
            t.what,
            a.stderr
        );
        assert_eq!(
            objects(),
            0,
            "{}: A's call did not reach the provider",
            t.what
        );
        std::fs::write(&release, "").unwrap();
        let b = finish(b).success();
        assert!(
            !b.stdout.contains("apply: complete"),
            "{}: {}",
            t.what,
            b.stdout
        );
    }
}

/// A bucket whose server ignores the conditions of a write is refused
/// before anything is written; one that keeps them is checked once.
#[test]
fn a_server_that_ignores_conditions_is_refused() {
    let lax = dform_s3::fake::Server::ignoring_conditions();
    let t = Target {
        what: "lax",
        endpoint: lax.endpoint.clone(),
        bucket: "dform-test".into(),
        id: "fake".into(),
        secret: "fake".into(),
    };
    let p = Project::new(&t, "lax");
    let r = p.run(APPLY).failure();
    assert!(
        r.stderr.contains("the server ignores If-None-Match: *"),
        "{}",
        r.stderr
    );
    assert_eq!(p.store().list("").unwrap(), Vec::<String>::new());
    // A plan without --out that needs an approval writes the plan key
    // (its digest is keyed): it is checked first too.
    let p = Project::of(&t, "lax-approval", |s| {
        s.write(
            "stacks/app.df",
            "\nuse fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nrequires_approval(r, \"every change\") where deformation(_, r, _)\n",
        );
    });
    let r = p.run(&["plan", "app"]).failure();
    assert!(
        r.stderr.contains("the server ignores If-None-Match: *"),
        "{}",
        r.stderr
    );
    assert_eq!(p.bucket("").list("").unwrap(), Vec::<String>::new());
    let good = &targets("a_server_that_ignores_conditions_is_refused")[0];
    let p = Project::new(good, "conditions-cached");
    p.run(APPLY).success();
    let cached = std::fs::read_dir(p.s.path("dform.state/cache/s3-conditions"))
        .unwrap()
        .count();
    assert_eq!(cached, 1);
}

/// A project reads another's outputs through its s3 backend: the project
/// mounted as a package names the bucket in its own dform.toml, and only
/// the published outputs object is read.
#[test]
fn a_project_reads_another_projects_outputs_through_its_s3_backend() {
    for t in &targets("a_project_reads_another_projects_outputs_through_its_s3_backend") {
        let p = Project::of(t, "remote", |s| {
            s.write(
                "stacks/cluster.df",
                "\n\
                 key env: string = \"dev\"\n\
                 use fake\n\
                 output endpoint = \"https://${env}.cluster.example\"\n\
                 ",
            );
        });
        p.run(&["apply", "cluster", "env=prod"]).success();
        let app = Scratch::project("s3-remote-app");
        app.write(
            "dform.toml",
            &format!(
                "[project]\nedition = \"2026\"\n\n[packages.platform]\npath = {:?}\n",
                p.s.dir.display().to_string()
            ),
        );
        app.write(
            "stacks/app.df",
            "\n\
             use fake\n\
             use platform.stacks.cluster\n\
             resource net.vpc edge {\n\
               name = e\n\
             } where e = cluster[env=\"prod\"].endpoint\n\
             ",
        );
        let mut c = p.command(&["plan", "app"], &[]);
        let r = Run::from(c.current_dir(&app.dir).output().unwrap()).success();
        assert!(
            r.stdout.contains("name = \"https://prod.cluster.example\""),
            "{}: {}",
            t.what,
            r.stdout
        );
    }
}

/// R-164: the bucket never holds a master in the clear. A new deployment
/// in a bucket with no `[secrets] passphrase` is refused before anything
/// is made; with one, no object in the bucket and no file under
/// dform.state holds the master (as bytes or hex) or a derived secret, and
/// a copy of the bucket opens nothing without the passphrase.
#[test]
fn the_bucket_never_holds_the_master_nor_a_derived_secret() {
    for t in &targets("the_bucket_never_holds_the_master_nor_a_derived_secret") {
        let setup = |s: &Scratch| {
            s.write(
                "stacks/p.df",
                "use fake\nresource db.secret v {\n  password = random.password(\"db\")\n}\n",
            );
            s.write(
                "providers/fake/schema.df",
                &(std::fs::read_to_string(
                    common::repo().join("crates/dform-mock/schemas/fake.df"),
                )
                .unwrap()
                    + "type_provider(db.secret, \"fakecloud\")\n\
                       type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n"),
            );
        };
        let p = Project::of(t, "custody", setup);
        p.run(&["apply", "p"]).success();
        let world: serde_json::Value =
            serde_json::from_str(&p.s.read("dform.state/p/remote.json")).unwrap();
        let pw = world["resources"]["db.secret::v"]["attrs"]["password"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: {world}", t.what))
            .to_string();
        let store = p.bucket("p");
        let keys = store.list("").unwrap();
        assert!(
            !keys.iter().any(|k| k == "state.key"),
            "{}: {keys:?}",
            t.what
        );
        let record: dform_core::custody::Record =
            serde_json::from_slice(&store.get("state.master").unwrap().unwrap().bytes).unwrap();
        let master = dform_core::custody::unseal(&record, PASSPHRASE.as_bytes())
            .unwrap()
            .expect("the passphrase opens it")
            .bytes();
        assert!(
            dform_core::custody::unseal(&record, b"not the passphrase")
                .unwrap()
                .is_none(),
            "{}",
            t.what
        );
        let hex: String = master.iter().map(|b| format!("{b:02x}")).collect();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for k in store.list("").unwrap() {
            files.push((k.clone(), store.get(&k).unwrap().unwrap().bytes));
        }
        fn local(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    local(&p, out);
                } else if p.file_name().is_some_and(|n| n != "remote.json") {
                    out.push((p.display().to_string(), std::fs::read(&p).unwrap()));
                }
            }
        }
        local(&p.s.path("dform.state"), &mut files);
        assert!(files.len() > 3, "{}: {files:?}", t.what);
        for (at, bytes) in &files {
            let text = String::from_utf8_lossy(bytes);
            assert!(
                !bytes.windows(32).any(|w| w == master) && !text.contains(&hex),
                "{}: {at} holds the master",
                t.what
            );
            assert!(!text.contains(&pw), "{}: {at} holds the password", t.what);
        }

        // Without a passphrase setting, a new deployment's master would
        // be a key file in the bucket: refused, nothing made.
        let bare = Project::of(t, "custody-bare", setup);
        let toml = bare.s.read("dform.toml");
        let toml = toml.replace(
            "[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
            "",
        );
        bare.s.write("dform.toml", &toml);
        let r = bare.run(&["apply", "p"]).failure();
        assert!(
            r.stderr.contains("a new master would be the key file")
                && r.stderr.contains("[secrets] passphrase"),
            "{}: {}",
            t.what,
            r.stderr
        );
        assert!(
            bare.bucket("p")
                .list("")
                .unwrap()
                .iter()
                .all(|k| k != "state.key"),
            "{}",
            t.what
        );
    }
}
