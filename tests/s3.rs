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
use common::{Run, Scratch};
use dform_core::store::{LOCK, S3Spec, STATE, Store};
use dform_s3::S3Store;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
        let s = Scratch::new(&format!("s3-{name}"));
        common::copy_dir(&common::repo().join("examples/demo"), &s.dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let prefix = format!("dform-test-{}-{nanos:x}-{name}", std::process::id());
        let mut toml = std::fs::read_to_string(s.path("dform.toml")).unwrap();
        toml.push_str(&format!(
            "\n[defaults]\nbackend = 's3(\"{}\", \"{prefix}/{{stack}}\", \
             {{endpoint: \"{}\", region: \"us-east-1\"}})'\n\
             lease_duration = \"{}ms\"\nlease_renewal = \"500ms\"\n",
            t.bucket,
            t.endpoint,
            LEASE.as_millis()
        ));
        std::fs::write(s.path("dform.toml"), toml).unwrap();
        let p = Project { t, s, prefix };
        p.store().create_bucket().unwrap();
        p
    }

    /// The deployment `dform[env=staging]`'s objects.
    fn store(&self) -> S3Store {
        let spec = S3Spec {
            bucket: self.t.bucket.clone(),
            prefix: format!("{}/dform", self.prefix),
            endpoint: Some(self.t.endpoint.clone()),
            region: Some("us-east-1".into()),
        };
        S3Store::with_credentials(
            &spec,
            "env=staging",
            rusty_s3::Credentials::new(&self.t.id, &self.t.secret),
        )
        .unwrap()
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = common::dform();
        c.args(args)
            .current_dir(&self.s.dir)
            .env("DFORM_S3_ACCESS_KEY_ID", &self.t.id)
            .env("DFORM_S3_SECRET_ACCESS_KEY", &self.t.secret);
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

    fn lease(&self) -> Option<serde_json::Value> {
        let o = self.store().get(LOCK).unwrap()?;
        Some(serde_json::from_slice(&o.bytes).unwrap())
    }
}

impl Drop for Project<'_> {
    fn drop(&mut self) {
        let store = self.store();
        let all = S3Store::with_credentials(
            &S3Spec {
                bucket: self.t.bucket.clone(),
                prefix: self.prefix.clone(),
                endpoint: Some(self.t.endpoint.clone()),
                region: Some("us-east-1".into()),
            },
            "",
            rusty_s3::Credentials::new(&self.t.id, &self.t.secret),
        )
        .unwrap();
        drop(store);
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
            plan.stdout.contains("+ net.vpc.network.main::vpc"),
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
            again.stdout.contains("undeformed"),
            "{}: {}",
            t.what,
            again.stdout
        );
        // Everything the local backend keeps is in the bucket, nothing of it
        // beside the project (the mock's world is the provider's, and stays).
        let keys = p.store().list("").unwrap();
        for k in ["state.json", "state.key", "state.audit.jsonl", "state.lock"] {
            assert!(keys.iter().any(|x| x == k), "{}: {k} in {keys:?}", t.what);
        }
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
            a.stdout.contains("apply: complete"),
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
            b.stdout
                .contains("resuming the apply interrupted at tick 1"),
            "{}: {}",
            t.what,
            b.stdout
        );
        assert!(
            b.stdout.contains("apply: complete"),
            "{}: {}",
            t.what,
            b.stdout
        );
        let st = p.state();
        assert_eq!(st["fence"], 2, "{}", t.what);
        assert!(st.get("in_flight").is_none(), "{}: {st}", t.what);
        let plan = p.run(PLAN).success();
        assert!(
            plan.stdout.contains("undeformed"),
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
            b.stdout.contains("resuming the apply"),
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
            b.stdout.contains("apply: complete"),
            "{}: {}",
            t.what,
            b.stdout
        );
        let plan = p.run(PLAN).success();
        assert!(
            plan.stdout.contains("undeformed"),
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

/// What does not take an s3 stack yet says so, naming the backend; the
/// stack list says where its state is.
#[test]
fn the_controller_and_rekey_refuse_an_s3_stack() {
    let t = &targets("the_controller_and_rekey_refuse_an_s3_stack")[0];
    let p = Project::new(t, "refuse");
    let r = p
        .run(&["controller", "run", "--once", "dform", "env=staging"])
        .failure();
    assert!(
        r.stderr
            .contains("stack dform[env=staging]: its backend is s3://dform-test/")
            && r.stderr.contains("does not run an s3 stack yet"),
        "{}",
        r.stderr
    );
    let r = p
        .run(&["stack", "rekey", "dform", "env=staging", "env=prod"])
        .failure();
    assert!(
        r.stderr.contains("rekey moves local state only"),
        "{}",
        r.stderr
    );
    let r = p.run(&["stack", "list"]).success();
    assert!(
        r.stdout.contains("  state in s3://dform-test/"),
        "{}",
        r.stdout
    );
}
