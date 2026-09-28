//! `stack` and `provider` statements: the stack's name scopes its state,
//! its backend holds the state and a lock, other stacks read its outputs,
//! and `provider` picks the mock's schema.

mod common;
use common::Scratch;
use std::process::{Command, Stdio};

const NET: &str = r#"edition 2026.
stack net.shared {}.
resource net.vpc main { cidr = "10.0.0.0/16" }.
output vpc_cidr = "10.0.0.0/16".
output vpc_id = ref(net.vpc, main, .id).
"#;

#[test]
fn the_stack_name_scopes_the_state() {
    let s = Scratch::new("lang-stack-name");
    s.write("net.df", NET);
    s.run(&["--file", "net.df", "apply"]).success();
    assert!(s.path(".dform/net.shared/state.json").exists());
    assert!(!s.path(".dform/net").exists());
    let r = s.run(&["--file", "net.df", "plan"]).success();
    assert_eq!(
        r.summary(),
        "stack net.shared is undeformed",
        "{}",
        r.stdout
    );
}

#[test]
fn a_local_backend_holds_the_state() {
    let s = Scratch::new("lang-stack-backend");
    s.write(
        "p.df",
        "edition 2026.\nstack x { backend = local(\"state/x\"), unknowns = permissive }.\nresource net.vpc main { cidr = \"10.0.0.0/16\" }.\n",
    );
    s.run(&["--file", "p.df", "apply"]).success();
    assert!(s.path("state/x/state.json").exists());
    assert!(s.path("state/x/remote.json").exists());
    assert!(
        !s.path("state/x/state.lock").exists(),
        "the lock is released"
    );
}

#[test]
fn one_program_owns_one_stack() {
    let s = Scratch::new("lang-stack-two");
    s.write("p.df", "edition 2026.\nstack a {}.\nstack b {}.\n");
    let r = s.run(&["--file", "p.df", "plan"]).failure();
    assert!(
        r.stderr
            .contains("p.df:3:1: a second stack statement: one program owns one stack"),
        "{}",
        r.stderr
    );
    s.write("p.df", "edition 2026.\nstack a { backend = s3(\"b\") }.\n");
    let r = s.run(&["--file", "p.df", "plan"]).failure();
    assert!(r.stderr.contains("unknown backend"), "{}", r.stderr);
}

/// Two processes: the first apply holds the stack's lock (a test hook
/// keeps it there until a file appears); the second fails cleanly naming
/// the holder; once the first is done the second goes through.
#[test]
fn a_second_concurrent_apply_fails_cleanly() {
    let s = Scratch::new("lang-stack-lock");
    s.write("net.df", NET);
    let first = Command::new(env!("CARGO_BIN_EXE_dform"))
        .args(["--file", "net.df", "apply"])
        .current_dir(&s.dir)
        .env("DFORM_TEST_HOLD_LOCK", s.path("release"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock = s.path(".dform/net.shared/state.lock");
    for _ in 0..1000 {
        if lock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(lock.exists(), "the first apply took the lock");
    let r = s.run(&["--file", "net.df", "apply"]).failure();
    assert!(
        r.stderr.contains(&format!(
            "stack net.shared is locked by another apply (pid {})",
            first.id()
        )),
        "{}",
        r.stderr
    );
    s.write("release", "");
    let out = first.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!lock.exists());
    let r = s.run(&["--file", "net.df", "apply"]).success();
    assert!(r.stdout.contains("apply: nothing to do"), "{}", r.stdout);
}

/// A lock left by an apply that is gone (killed) is taken over.
#[test]
fn a_stale_lock_is_taken_over() {
    let s = Scratch::new("lang-stack-stale");
    s.write("net.df", NET);
    let mut dead = Command::new("true").spawn().unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    s.write(".dform/net.shared/state.lock", &format!("{pid}\n"));
    let r = s.run(&["--file", "net.df", "apply"]).success();
    assert!(
        r.stderr
            .contains(&format!("taking over the lock of pid {pid}, which is gone")),
        "{}",
        r.stderr
    );
}

/// An applied stack's outputs, computed values included once they exist,
/// are `stack_output` facts for every other stack.
#[test]
fn another_stack_reads_the_outputs() {
    let s = Scratch::new("lang-stack-outputs");
    s.write("net.df", NET);
    s.write(
        "app.df",
        r#"edition 2026.
stack app {}.
resource net.subnet a { cidr = C, vpc_id = V } :-
  stack_output("net.shared", vpc_cidr, C),
  stack_output("net.shared", vpc_id, V).
"#,
    );
    let r = s.run(&["--file", "app.df", "plan"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);
    s.run(&["--file", "net.df", "apply"]).success();
    let r = s.run(&["--file", "app.df", "plan"]).success();
    assert!(
        r.stdout
            .contains("+ net.subnet.a\n  cidr = \"10.0.0.0/16\"\n  vpc_id = \"net.vpc:main\"\n"),
        "{}",
        r.stdout
    );
}

/// `provider` picks the mock's schema; `--provider` overrides it.
#[test]
fn the_provider_statement_selects_the_schema() {
    let s = Scratch::new("lang-stack-provider");
    s.write(
        "mine/schema.df",
        "type_provider(x.thing, fakecloud).\ntype_attr(x.thing, size, int, [required]).\n",
    );
    s.write(
        "p.df",
        "edition 2026.\nprovider mine { source = \"mine\" }.\nresource x.thing a {}.\n",
    );
    let r = s.run(&["--file", "p.df", "plan"]).failure();
    assert!(r.stderr.contains("size"), "{}", r.stderr);
    let r = s
        .run(&["--file", "p.df", "--provider", "fake", "plan"])
        .success();
    assert!(r.stdout.contains("+ x.thing.a"), "{}", r.stdout);
}
