//! `stack` and `provider` statements: the stack's name scopes its state,
//! its backend holds the state and a lock, other stacks read its outputs,
//! and `provider` picks the mock's schema.

mod common;
use common::Scratch;
use std::process::{Command, Stdio};

const NET: &str = r#"edition 2026
stack net.shared {}
resource net.vpc main { cidr = "10.0.0.0/16" }
output vpc_cidr = "10.0.0.0/16"
output vpc_id = ref(net.vpc, "main", .id)
"#;

#[test]
fn the_stack_name_scopes_the_state() {
    let s = Scratch::new("lang-stack-name");
    s.write("net.df", NET);
    s.run(&["apply", "net.df"]).success();
    assert!(s.path("dform.state/net.shared/state.json").exists());
    assert!(!s.path("dform.state/net").exists());
    let r = s.run(&["plan", "net.df"]).success();
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
        "edition 2026\nstack x { backend = local(\"state/x\"), unknowns = \"permissive\" }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["apply", "p.df"]).success();
    assert!(s.path("state/x/state.json").exists());
    assert!(s.path("state/x/remote.json").exists());
    assert!(
        !s.path("state/x/state.lock").exists(),
        "the lock is released"
    );
}

/// `local(DIR)` is relative to the project root (the directory holding
/// `dform.toml`, and `dform.state/`), found up from the working directory,
/// and so is `handover --to local(DIR)`.
#[test]
fn a_local_backend_is_relative_to_the_project_root() {
    let s = Scratch::new("lang-stack-backend-root");
    s.write("infra/dform.toml", "");
    s.write(
        "infra/stacks/p.df",
        "edition 2026\nstack x { backend = local(\"state/x\") }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["-C", "infra", "apply", "x"]).success();
    assert!(s.path("infra/state/x/state.json").exists());
    assert!(!s.path("state").exists());
    // From a subdirectory of the project: the same root.
    let r = s.run_in("infra/stacks", &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack x is undeformed", "{}", r.stdout);
    assert!(!s.path("infra/stacks/state").exists());

    s.write("infra/stacks/net.df", NET);
    s.run(&["-C", "infra", "apply", "net.shared"]).success();
    s.run(&[
        "-C",
        "infra",
        "stack",
        "handover",
        "net.shared",
        "--to",
        "local(\"moved\")",
    ])
    .success();
    assert!(s.path("infra/moved/state.json").exists());
    assert!(!s.path("moved").exists());
    let r = s.run(&["-C", "infra", "plan", "net.shared"]).success();
    assert_eq!(
        r.summary(),
        "stack net.shared is undeformed",
        "{}",
        r.stdout
    );
}

#[test]
fn one_program_owns_one_stack() {
    let s = Scratch::new("lang-stack-two");
    s.write("p.df", "edition 2026\nstack a {}\nstack b {}\n");
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("p.df:3:1: a second stack statement: one program owns one stack"),
        "{}",
        r.stderr
    );
    s.write("p.df", "edition 2026\nstack a { backend = s3(\"b\") }\n");
    let r = s.run(&["plan", "p.df"]).failure();
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
        .args(["apply", "net.df"])
        .current_dir(&s.dir)
        .env("DFORM_TEST_HOLD_LOCK", s.path("release"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock = s.path("dform.state/net.shared/state.lock");
    for _ in 0..1000 {
        if lock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(lock.exists(), "the first apply took the lock");
    let r = s.run(&["apply", "net.df"]).failure();
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
    let r = s.run(&["apply", "net.df"]).success();
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
    s.write("dform.state/net.shared/state.lock", &format!("{pid}\n"));
    let r = s.run(&["apply", "net.df"]).success();
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
        r#"edition 2026
stack app {}
resource net.subnet a {
  for stack_output("net.shared", "vpc_cidr", c),
    stack_output("net.shared", "vpc_id", v)
  cidr = c
  vpc_id = v
}
"#,
    );
    let r = s.run(&["plan", "app.df"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);
    s.run(&["apply", "net.df"]).success();
    let r = s.run(&["plan", "app.df"]).success();
    assert!(
        r.stdout
            .contains("+ net.subnet.a\n  cidr = \"10.0.0.0/16\"\n  vpc_id = \"net.vpc:main\"\n"),
        "{}",
        r.stdout
    );
}

/// The registry and the stacks' state live at the project root
/// (`dform.state/`), found from the working directory: stacks that read each
/// other's outputs share them from anywhere in the project. `-C DIR` runs
/// as if from DIR; the registry records absolute state paths.
#[test]
fn the_registry_is_the_projects() {
    let s = Scratch::new("lang-stack-root");
    s.write("infra/dform.toml", "");
    s.write("infra/stacks/net.df", NET);
    let app = r#"edition 2026
stack app {}
resource net.subnet a {
  for stack_output("net.shared", "vpc_cidr", c),
    stack_output("net.shared", "vpc_id", v)
  cidr = c
  vpc_id = v
}
"#;
    s.write("infra/stacks/app.df", app);
    s.run(&["-C", "infra", "apply", "net.shared"]).success();
    assert!(s.path("infra/dform.state/stacks.json").exists());
    assert!(!s.path("dform.state").exists());
    let registry: serde_json::Value =
        serde_json::from_str(&s.read("infra/dform.state/stacks.json")).unwrap();
    let state = registry["net.shared"].as_str().unwrap();
    assert!(std::path::Path::new(state).is_absolute(), "{registry}");

    let want = "+ net.subnet.a\n  cidr = \"10.0.0.0/16\"\n";
    let r = s.run_in("infra/stacks", &["plan", "app"]).success();
    assert!(r.stdout.contains(want), "{}", r.stdout);

    // Another project sees none of it; -C runs in this one.
    s.write("elsewhere/app.df", app);
    let r = s.run_in("elsewhere", &["plan", "app.df"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);
    let r = s
        .run_in("elsewhere", &["-C", "../infra", "plan", "app"])
        .success();
    assert!(r.stdout.contains(want), "{}", r.stdout);
}

/// `provider` picks the mock's schema; `--provider` overrides it.
#[test]
fn the_provider_statement_selects_the_schema() {
    let s = Scratch::new("lang-stack-provider");
    s.write(
        "mine/schema.df",
        "type_provider(x.thing, \"fakecloud\")\ntype_attr(x.thing, \"size\", \"int\", [\"required\"])\n",
    );
    s.write(
        "p.df",
        "edition 2026\nprovider mine { source = \"mine\" }\nresource x.thing a {}\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(r.stderr.contains("size"), "{}", r.stderr);
    let r = s
        .run(&["dev", "--provider", "fake", "plan", "p.df"])
        .success();
    assert!(r.stdout.contains("+ x.thing.a"), "{}", r.stdout);
}
